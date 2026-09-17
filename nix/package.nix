{
  lib,
  rustPlatform,
  fetchFromGitHub,
  zig_0_16,
  stdenv,
  darwin,
  xcbuild,
  linkFarm,
  fetchzip,
  fetchurl,
  fetchgit,
  runCommandLocal,
  zstd,
  src,
}:
let
  ghosttyCommit = "ab0b9da9e88fcb4b0533a1854e84628f663930af";

  ghosttySrc =
    assert lib.assertMsg
      (lib.hasInfix ghosttyCommit (builtins.readFile "${src}/vendor/libghostty-vt-sys/build.rs"))
      "ghosttyCommit in nix/package.nix no longer matches GHOSTTY_COMMIT in vendor/libghostty-vt-sys/build.rs; update the rev and hash here to match";
    fetchFromGitHub {
      owner = "ghostty-org";
      repo = "ghostty";
      rev = ghosttyCommit;
      hash = "sha256-LZuEFAt3/wfn6YWfk7NHnqLAtZ9g4mi6yTptx0mLKj0=";
    };

  # Ghostty commits a zon2nix-generated `build.zig.zon.nix` that fetches its
  # Zig package deps as fixed-output derivations. The resulting directory has
  # the layout `zig build --system` expects, so libghostty-vt-sys's build.rs
  # can resolve packages from it instead of fetching over the network.
  ghosttyZigDeps = import "${ghosttySrc}/build.zig.zon.nix" {
    inherit
      lib
      linkFarm
      fetchzip
      fetchurl
      fetchgit
      runCommandLocal
      zstd
      zig_0_16
      ;
    name = "fut-ghostty-zig-deps";
  };
in
rustPlatform.buildRustPackage rec {
  pname = "fut";
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [
    zig_0_16
  ]
  ++ lib.optionals stdenv.hostPlatform.isDarwin [
    darwin.cctools
    xcbuild
  ];

  env = {
    GHOSTTY_SOURCE_DIR = "${ghosttySrc}";
    GHOSTTY_ZIG_SYSTEM_DIR = "${ghosttyZigDeps}";
  };

  # zig_0_16's setup hook claims configurePhase/buildPhase/installPhase for
  # itself whenever they're unset, racing cargoBuildHook for the top-level
  # phases. Zig here only runs as a subprocess of build.rs, so keep cargo in
  # charge of the actual phases.
  dontUseZigConfigure = true;
  dontUseZigBuild = true;
  dontUseZigInstall = true;
  dontUseZigCheck = true;

  # `zig env`, called internally by the build runner, writes its cache dir
  # under $HOME regardless of --global-cache-dir. The sandbox's fake $HOME
  # is read-only, so give it a writable one.
  preBuild = ''
    export HOME=$(mktemp -d)
  '';

  # The test suite spawns a daemon over a Unix socket and drives real PTYs,
  # which the Nix build sandbox does not support.
  doCheck = false;

  meta = {
    description = "Agent-aware terminal multiplexer";
    homepage = "https://fut.sh";
    mainProgram = "fut";
    platforms = lib.platforms.unix;
  };
}
