{ system, stdenv, fetchzip, }:

let
  version = "27.1";
  arch = if system == "aarch64-linux" then "linux-aarch_64" else "linux-x86_64";
  hash = {
    "x86_64-linux" = "sha256-jk1VHYxOMo7C6mr1EVL97I2+osYz7lRtQLULv91gFH4=";
    "aarch64-linux" = "sha256-ozZBHlgEiRycPiYH1aLb9QkvGmO3qY3+cLmsC/OrZB4=";
  }.${system};

in stdenv.mkDerivation {
  pname = "protoc";
  inherit version;

  src = fetchzip {
    url = "https://github.com/protocolbuffers/protobuf/releases/download/v${version}/protoc-${version}-${arch}.zip";
    stripRoot = false;
    inherit hash;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp bin/protoc $out/bin/
    cp -r include $out
    runHook postInstall
  '';
}
