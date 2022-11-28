{
  stdenv,
  lib,
  rust,
  rustPlatform,
  fetchgit,
  fetchpatch,
  clang,
  pkg-config,
  protobuf,
  python3,
  wayland-scanner,
  libcap,
  libdrm,
  libepoxy,
  minijail,
  virglrenderer,
  wayland,
  wayland-protocols,
  src,
  version,
  doCheck ? false,
}:
rustPlatform.buildRustPackage rec {
  pname = "crosvm";

  inherit doCheck src version;

  separateDebugInfo = true;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  nativeBuildInputs =
    [clang pkg-config protobuf python3]
    ++ lib.optionals (!stdenv.hostPlatform.isRiscV) [wayland-scanner];

  buildInputs =
    [
      libcap
      minijail
    ]
    ++ lib.optionals (!stdenv.hostPlatform.isRiscV) [
      libdrm
      libepoxy
      virglrenderer
      wayland
      wayland-protocols
    ];

  preConfigure = ''
    patchShebangs third_party/minijail/tools/*.py
    # uncomment once rebased on main
    # substituteInPlace build.rs --replace '"clang"' '"${stdenv.cc.targetPrefix}clang"'
  '';

  "CARGO_TARGET_${lib.toUpper (builtins.replaceStrings ["-"] ["_"] (rust.toRustTarget stdenv.hostPlatform))}_LINKER" = "${stdenv.cc.targetPrefix}cc";

  # crosvm mistakenly expects the stable protocols to be in the root
  # of the pkgdatadir path, rather than under the "stable"
  # subdirectory.
  PKG_CONFIG_WAYLAND_PROTOCOLS_PKGDATADIR = "${wayland-protocols}/share/wayland-protocols/stable";

  buildNoDefaultFeatures = true;
  buildFeatures = lib.optionals (!stdenv.hostPlatform.isRiscV) ["default" "virgl_renderer" "virgl_renderer_next"];

  meta = with lib; {
    description = "A secure virtual machine monitor for KVM";
    homepage = "https://chromium.googlesource.com/crosvm/crosvm/";
    maintainers = with maintainers; [qyliss];
    license = licenses.bsd3;
    platforms = ["aarch64-linux" "riscv64-linux" "x86_64-linux"];
  };
}
