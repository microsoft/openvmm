{ system, stdenv, fetchzip, }:

let
  version = "0.17.0";
  arch = if system == "aarch64-linux" then "aarch64-unknown-linux-musl" else "x86_64-unknown-linux-gnu";
  hash = {
    "x86_64-linux" = "sha256-eqO3OU9VPCSN+1zfqK0aOkAvJ7tmB7W/ieDLPejJYV4=";
    "aarch64-linux" = "sha256-3TSbAzUZfX/oEut0AVLuAHQlahoJaonZp2lTpBkg2q0=";
  }.${system};

in stdenv.mkDerivation {
  pname = "mdbook-mermaid";
  inherit version;

  src = fetchzip {
    url = "https://github.com/badboy/mdbook-mermaid/releases/download/v${version}/mdbook-mermaid-v${version}-${arch}.tar.gz";
    inherit hash;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp mdbook-mermaid $out/bin/
    runHook postInstall
  '';
}
