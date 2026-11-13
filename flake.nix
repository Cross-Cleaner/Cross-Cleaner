# Nix flake for Cross Cleaner: builds all three Linux front-ends from this
# workspace with the `self-update` feature compiled out, so a Nix install
# cannot rewrite its own binaries (`/nix/store` would reject that anyway).
#
# Usage from anywhere:
#   nix run github:Cross-Cleaner/Cross-Cleaner            # window app
#   nix run github:Cross-Cleaner/Cross-Cleaner#cross-cleaner-tui
#   nix build github:Cross-Cleaner/Cross-Cleaner           # ./result/bin/*
#   nix profile install github:Cross-Cleaner/Cross-Cleaner
#
# The derivation here is deliberately inline: when it is ready for the
# nixpkgs contribution process (pkgs/by-name), lift it into package.nix and
# register it there — see the discussion elsewhere in this repository.
{
  description = "Cross Cleaner — removes temporary files, cache and other system junk (GUI, TUI and CLI)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      # Linux only: the release workflow publishes Windows, macOS and Android
      # binaries separately, and this workspace's Linux path needs the same
      # runtime libraries as the .deb / .rpm (see packaging/linux/nfpm.yaml).
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          package = pkgs.callPackage
            # callPackage supplies every argument except the ones that only
            # this flake can provide.
            ({ lib, rustPlatform, makeWrapper, imagemagick, pkg-config
             , alsa-lib, libdrm, libGL, mesa, libxkbcommon, wayland, fontconfig, xorg
             }:

              let
                # The full application version as on the release tag (v2.0.4.4).
                # The fourth component — and the fifth one on a quick patch fix
                # (2.0.4.4.1) — is chosen in the release workflow; Cargo.toml on
                # the branch carries only the first three (2.0.4), so there is
                # nothing to derive it from. Bump this by hand on every release.
                #
                # A `let`, not an attribute of buildRustPackage: `env` below
                # reads this value, and plain (non-`rec`) attrset members do
                # not see each other.
                version = "2.0.4.7";
              in
              rustPlatform.buildRustPackage {
                pname = "cross-cleaner";
                # stdenv composes the derivation name as "${pname}-${version}"
                # from these two attributes only; the let-binding above is not
                # seen by the attrset itself, so inherit it explicitly.
                inherit version;

                src = ./.;

                # The binaries read the version from the APP_VERSION env var at
                # compile time (database::get_version / winicon build script);
                # the release workflow does the same, and without it `--version`
                # would fall back to a stale hardcoded string.
                env.APP_VERSION = version;

                cargoLock.lockFile = ./Cargo.lock;
                # Every dependency comes from crates.io (the gpu-allocator git
                # patch went away once its crates.io release gained the
                # widened windows requirement), so no outputHashes are needed
                # here and the lock file alone pins the vendor tree.

                # Same rule as the .deb / .rpm: a package-manager install owns
                # its version, so distribution builds compile the updater out
                # entirely (`--no-default-features` drops dep:selfupdate).
                buildNoDefaultFeatures = true;

                # Building the whole workspace would also build the Android
                # edition, whose host-side tooling (the ndk toolchain) is
                # pointless on Linux; build the three Linux front-ends
                # explicitly, exactly like the release workflow does.
                cargoBuildFlags = [ "-p" "desktop" "-p" "tui" "-p" "cli" ];

                # `cargo test` is skipped in the flake (set doCheck = true to
                # bring it back; `cargo test` runs the same three packages).
                doCheck = false;

                # `alsa-sys` links ALSA through pkg-config at compile time (the
                # -sys crates insist on a pkg-config probe): without it the
                # cpal build panics with "The pkg-config command could not be
                # found". The other multimedia libs are dlopen()ed at runtime
                # and come via LD_LIBRARY_PATH in postFixup instead.
                nativeBuildInputs = [ makeWrapper imagemagick pkg-config ];
                buildInputs = [ alsa-lib ];

                postInstall = ''
                  # Cargo installs the bins under the crate names; the desktop
                  # entries and every user expect the stable cross-cleaner*
                  # names the packages use.
                  mv $out/bin/desktop $out/bin/cross-cleaner
                  mv $out/bin/tui      $out/bin/cross-cleaner-tui
                  mv $out/bin/cli      $out/bin/cross-cleaner-cli

                  # The desktop entries live in packaging/linux; the headless
                  # CLI gets no entry on purpose — same choice as the .deb.
                  install -Dm444 packaging/linux/cross-cleaner.desktop \
                    $out/share/applications/cross-cleaner.desktop
                  install -Dm444 packaging/linux/cross-cleaner-tui.desktop \
                    $out/share/applications/cross-cleaner-tui.desktop

                  # hicolor icon set from the bundled .ico, the same sizes the
                  # .deb installs. 256 is the master frame; the rest are the
                  # downscales the icon cache prefers. magick does not create
                  # the target directories, seed them first.
                  for size in 48 96 128 256; do
                    install -dm755 "$out/share/icons/hicolor/''${size}x''${size}/apps"
                    magick -background none crates/winicon/assets/icon.ico \
                      -filter Lanczos -resize ''${size}x''${size} \
                      PNG32:$out/share/icons/hicolor/''${size}x''${size}/apps/cross-cleaner.png
                  done
                '';

                # winit reaches the whole X11/Wayland stack and cpal reaches
                # ALSA through dlopen(), so none of these appear in `ldd` and
                # the same hand-maintained list as nfpm.yaml must be wrapped
                # into LD_LIBRARY_PATH for the binaries to run outside FHS.
                postFixup =
                  let
                    runtimeLibs = lib.makeLibraryPath [
                      alsa-lib
                      libdrm
                      libGL
                      (lib.getLib mesa)   # libEGL, which glutin probes on Wayland
                      libxkbcommon
                      wayland
                      fontconfig
                      xorg.libX11
                      xorg.libXcursor
                      xorg.libXrandr
                      xorg.libXi
                      xorg.libXinerama
                      xorg.libXext
                      xorg.libXrender
                    ];
                  in
                  ''
                    for bin in cross-cleaner cross-cleaner-tui cross-cleaner-cli; do
                      wrapProgram $out/bin/$bin \
                        --prefix LD_LIBRARY_PATH : ${runtimeLibs}
                    done
                  '';

                meta = {
                  description =
                    "Remove temporary files, cache and other system junk";
                  longDescription = ''
                    Cross Cleaner removes temporary files, caches and leftover
                    system junk. It ships as a native window app, a terminal
                    app and a command-line tool, all sharing one cleaning
                    database, and cleans Windows, macOS, Linux and Android on
                    x86_64 and arm64 alike. This build compiles without the
                    self-update feature: Nix owns the installed version.
                  '';
                  homepage = "https://cross-cleaner.github.io/";
                  license = lib.licenses.gpl3Plus;
                  mainProgram = "cross-cleaner";
                  platforms = lib.platforms.linux;
                };
              })
            { };
        in
        {
          cross-cleaner = package;
          default = package;
        });

      apps = forAllSystems (system:
        let
          pkg = self.packages.${system}.default;
          app = { type = "app"; program = "${pkg}/bin/cross-cleaner"; };
        in
        {
          default = app;
          cross-cleaner = app;
          cross-cleaner-tui = { type = "app"; program = "${pkg}/bin/cross-cleaner-tui"; };
          cross-cleaner-cli = { type = "app"; program = "${pkg}/bin/cross-cleaner-cli"; };
        });

      # `nixpkgs.overlays = [ (import github:Cross-Cleaner/Cross-Cleaner) ]`
      # — the package keeps its own nixpkgs pin, matching what git-pinned
      # flakes do.
      overlays.default = final: prev: {
        cross-cleaner = self.packages.${final.system}.default;
      };
    };
}
