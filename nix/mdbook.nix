{ system, stdenv, fetchzip, }:

let
  version = "0.4.40";
  arch = if system == "aarch64-linux" then "aarch64-unknown-linux-musl" else "x86_64-unknown-linux-gnu";
  hash = {
    "x86_64-linux" = "sha256-ijQbAOvEcmKaoPMe+eZELxY8iCJvrMnk4R07+d5lGtQ=";
    "aarch64-linux" = "sha256-YhPCIdU8F84fZ7zDvbPP57MnPXR3DL2T9GjXDV5akz8=";
  }.${system};

in stdenv.mkDerivation {
  pname = "mdbook";
  inherit version;

  src = fetchzip {
    url = "https://github.com/rust-lang/mdBook/releases/download/v${version}/mdbook-v${version}-${arch}.tar.gz";
    inherit hash;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp mdbook $out/bin/
    runHook postInstall
  '';
}
