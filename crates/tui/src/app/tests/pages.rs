use super::*;

/// Every action the window frontend gives a sound must reach the same clip
/// here.
///
/// The terminal app shipped silent while `gui` played five clips, so the map
/// is pinned rather than left to whoever reads the code: a binding that
/// quietly stops making noise is a defect the user has to notice, not one a
/// test stumbles over.
#[test]
fn actions_play_the_same_clips_as_the_window_frontend() {
    // This one runs a real cleaning job, so it reports through the same
    // process-global diagnostics queue the test below owns. Take turns, and
    // capture the output so it never reaches stderr either.
    let _serial = sequential();
    let _guard = DiagnosticsGuard::install();
    let mut app = sample_app();
    take_played();

    // Category checkbox: selecting and clearing are two different clips.
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(take_played(), ["check", "uncheck"], "category checkbox");

    // Both categories, so Firefox really spans two of them — that is what
    // makes the per-program popup reachable at all. The grid is two columns
    // wide, so Tab is what reaches the second category; Down would stay on
    // the only row.
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Char(' '));
    // `n` is the window frontend's Next button.
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.state.current_page, Page::ProgramSelection);
    assert_eq!(
        take_played(),
        ["check", "check", "click"],
        "both categories, then Next",
    );

    // Program checkbox. The list arrives pre-selected, so this clears and
    // re-selects rather than the other way round.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    assert_eq!(take_played(), ["uncheck", "check"], "program checkbox");

    // `→` is the per-program category menu; the sample Chrome entry is in a
    // single category, so the refusal must stay silent.
    press(&mut app, KeyCode::Right);
    assert!(app.popup.is_none(), "Chrome has one category");
    assert_eq!(take_played(), Vec::<&str>::new(), "refused popup");

    // Firefox is in two, so this one opens.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    assert!(app.popup.is_some());
    assert_eq!(take_played(), ["pop"], "program category menu");

    // The popup starts with every category enabled, so the first space
    // excludes one rather than adding it.
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(take_played(), ["uncheck", "check"], "popup entry");
    // Esc is navigation, not a button: dismissing the overlay is silent.
    press(&mut app, KeyCode::Esc);
    assert!(app.popup.is_none());
    assert_eq!(take_played(), Vec::<&str>::new(), "escape");

    // `S` starts the run. It spawns onto a tokio runtime, and the sample
    // entries have nothing to remove, so no real file is touched.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        press(&mut app, KeyCode::Char('S'));
    });
    assert_eq!(take_played(), ["click"], "Start");

    // The job runs on the runtime's own threads, so the result arrives on
    // whichever one is free: `tick` until it lands.
    for _ in 0..500 {
        app.tick();
        if app.state.current_page == Page::Results {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        app.state.current_page,
        Page::Results,
        "the run never finished",
    );
    assert_eq!(take_played(), ["done"], "the finishing clip");
}

#[test]
fn search_narrows_the_program_list() {
    let mut app = sample_app();
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(app.input, InputMode::Editing);
    for c in "fox".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    assert_eq!(app.state.search_query_visible, "fox");
    assert_eq!(app.state.filtered_programs.len(), 1);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.input, InputMode::Normal);
    press(&mut app, KeyCode::Char('u'));
    assert_eq!(app.state.filtered_programs.len(), 2);
}

#[test]
fn backspace_rewrites_the_query() {
    let mut app = sample_app();
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('b'));
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.state.search_query_visible, "a");
}

#[test]
fn clearing_page_renders_a_spinner_before_the_first_tick() {
    let mut app = sample_app();
    app.state.current_page = Page::Clearing;
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("preparing"), "{screen}");
    // Once a tick arrives the bar replaces the spinner.
    app.state.total_tasks = 4;
    app.state.current_task = 2;
    app.state.cleaned_bytes = 2048;
    app.state.progress_start = Some(Instant::now());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("2/4"), "{screen}");
    assert!(screen.contains("2.0 KB"), "{screen}");
}

#[test]
fn results_page_renders_the_summary_and_table() {
    let mut app = sample_app();
    app.state.cleared_data = Some((
        4096,
        7,
        2,
        Arc::from(vec![database::structures::Cleared {
            program: "Chrome".to_string(),
            removed_bytes: 4096,
            removed_files: 7,
            removed_directories: 2,
            affected_categories: vec!["Cache".to_string(), "Logs".to_string()],
            category_totals: Vec::new(),
            paths: Vec::new(),
            paths_omitted: 0,
        }]),
    ));
    app.state.current_page = Page::Results;
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Cleaning Results"), "{screen}");
    assert!(screen.contains("4.0 KB"), "{screen}");
    assert!(screen.contains("Chrome"), "{screen}");
    assert!(screen.contains("Cache, Logs"), "{screen}");
}

#[test]
fn the_results_header_lines_up_with_its_columns() {
    // Values chosen so each column has a recognisable, differently sized
    // entry: if the header and the rows used different widths, the two
    // would not line up.
    let mut app = sample_app();
    app.state.cleared_data = Some((
        4096,
        7,
        2,
        Arc::from(vec![
            database::structures::Cleared {
                program: "Chrome".to_string(),
                removed_bytes: 4096,
                removed_files: 7,
                removed_directories: 2,
                affected_categories: vec!["Cache".to_string()],
                category_totals: Vec::new(),
                paths: Vec::new(),
                paths_omitted: 0,
            },
            database::structures::Cleared {
                program: "Visual Studio Code".to_string(),
                removed_bytes: 3_221_225_472,
                removed_files: 1234,
                removed_directories: 567,
                affected_categories: vec!["Cache".to_string(), "Logs".to_string()],
                category_totals: Vec::new(),
                paths: Vec::new(),
                paths_omitted: 0,
            },
        ]),
    ));
    app.state.current_page = Page::Results;
    let screen = draw(&mut app, 110, 24);
    let lines: Vec<&str> = screen.lines().collect();
    let header = header_line(&screen, &lines);

    let data: Vec<&str> = lines
        .iter()
        .filter(|line| column_of(line, "Chrome").is_some() || column_of(line, "Visual").is_some())
        .copied()
        .collect();
    assert_eq!(data.len(), 2, "expected two rows:\n{screen}");

    // The labels sit inside the top border, and the rule must survive
    // *between* them: a block title is drawn over the border, so any blank
    // inside it erases the line and leaves the labels floating.
    assert!(
        header.starts_with('╭') && header.trim_end().ends_with('╮'),
        "the top border must be drawn around the labels:\n{screen}",
    );
    let border = header.trim_start_matches('╭').trim_end_matches('╮');
    // Only the labels and the single space after each may interrupt the rule.
    let interruptions = border.chars().filter(|c| *c != '─' && *c != ' ').count();
    assert!(
        interruptions > 0,
        "the labels must be inside the border:\n{screen}",
    );
    // Between "Dirs" and "Categories" there is a rule, not a blank gap.
    let dirs = header.find("Dirs").expect("Dirs label");
    let categories = header.find("Categories").expect("Categories label");
    let between = &header[dirs + "Dirs".len()..categories];
    assert!(
        between.contains('─'),
        "the rule must show between the labels, found {between:?}:\n{screen}",
    );

    // Every column is left-aligned, so the header and the value must start
    // at the very same column.
    for ((label, short), (_, long)) in [
        (("Program", "Chrome"), ("Program", "Visual Studio Code")),
        (("Size", "4.0 KB"), ("Size", "3.0 GB")),
        (("Files", "7"), ("Files", "1234")),
        (("Dirs", "2"), ("Dirs", "567")),
        (("Categories", "Cache"), ("Categories", "Cache, Logs")),
    ] {
        let header_at =
            column_of(header, label).unwrap_or_else(|| panic!("header has no {label}:\n{screen}"));
        for (row, needle) in data.iter().zip([short, long]) {
            let row_at =
                column_of(row, needle).unwrap_or_else(|| panic!("row has no {needle}:\n{screen}"));
            assert_eq!(
                header_at, row_at,
                "{label}: header starts at {header_at}, {needle} at {row_at}\n{screen}",
            );
        }
    }
}

#[test]
fn results_page_without_data_falls_back_gracefully() {
    let mut app = sample_app();
    app.state.current_page = Page::Results;
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("No results yet"), "{screen}");
}

#[test]
fn settings_page_renders_all_four_volumes() {
    let mut app = sample_app();
    app.state.current_page = Page::Settings;
    let screen = draw(&mut app, 100, 30);
    for label in SETTINGS {
        assert!(screen.contains(label), "missing {label}:\n{screen}");
    }
}

#[test]
fn settings_volume_steps_and_clamps() {
    let _serial = config_lock();
    let mut app = sample_app();
    app.state.current_page = Page::Settings;
    for _ in 0..20 {
        press(&mut app, KeyCode::Left);
    }
    let cfg = appcore::config::get();
    assert_eq!(cfg.sound_volume, 0.0, "clamps at silence");
    press(&mut app, KeyCode::Right);
    assert!((appcore::config::get().sound_volume - 0.1).abs() < 1e-6);
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(appcore::config::get().sound_volume, 1.0, "resets to full");
    // Put the shared config back so the test does not leak state.
    appcore::config::update(|c| *c = AppConfig::default());
}

#[test]
fn escape_goes_back_then_quits_from_the_main_page() {
    let mut app = sample_app();
    app.state.current_page = Page::Settings;
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.state.current_page, Page::Main);
    assert!(!app.should_quit, "still on the main page");
    press(&mut app, KeyCode::Esc);
    assert!(app.should_quit);
}

#[test]
fn ctrl_c_always_quits() {
    let mut app = sample_app();
    app.on_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(app.should_quit);
}

#[test]
fn toast_replaces_the_key_hints() {
    let mut app = sample_app();
    press(&mut app, KeyCode::Char(' '));
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Category selected"), "{screen}");
    assert!(
        !screen.contains("changelog"),
        "hints are replaced: {screen}"
    );
}

#[test]
fn global_bindings_do_not_steal_page_keys() {
    let mut app = many_categories(6);
    // `j` / `k` move the cursor; only `G`, `O` and `?` are global.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.category_cursor, 2);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.category_cursor, 0);
    assert!(app.popup.is_none());
    assert!(!app.changelog_open);
}

#[test]
fn every_page_survives_a_tiny_terminal() {
    let mut app = sample_app();
    for page in [
        Page::Main,
        Page::ProgramSelection,
        Page::Clearing,
        Page::Results,
        Page::Settings,
    ] {
        app.state.current_page = page;
        // Must not panic on zero-sized or cramped layouts.
        let _ = draw(&mut app, 20, 6);
        let _ = draw(&mut app, 8, 3);
    }
}

#[test]
fn cursors_stay_inside_the_list_after_navigation() {
    let mut app = sample_app();
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::End);
    assert_eq!(app.program_cursor, 1);
    // A search that matches nothing empties the list and resets the cursor.
    press(&mut app, KeyCode::Char('/'));
    for c in "zzz".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    assert!(app.state.filtered_programs.is_empty());
    assert_eq!(app.program_cursor, 0);
    // Esc cancels the search, so the list comes back.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.state.search_query_visible, "");
    assert_eq!(app.state.filtered_programs.len(), 2);
    // Leaving the page must not leave a cursor pointing at a missing row.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    assert!(app.program_cursor < app.state.filtered_programs.len());
}
