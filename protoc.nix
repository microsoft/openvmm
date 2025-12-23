{ stdenv, fetchzip, }:

stdenv.mkDerivation {
  pname = "protoc";
  version = "27.1";

  src = fetchzip {
    url =
      "https://github.com/protocolbuffers/protobuf/releases/download/v27.1/protoc-27.1-linux-x86_64.zip";
    stripRoot = false;
    hash = "sha256-jk1VHYxOMo7C6mr1EVL97I2+osYz7lRtQLULv91gFH4=";
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp bin/protoc $out/bin/
    cp -r include $out
    runHook postInstall
  '';
}
