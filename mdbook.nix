{ stdenv, fetchzip,
#fetchFromGitHub
}:

stdenv.mkDerivation {
  pname = "mdBook";
  version = "0.4.40";

  # src = fetchFromGitHub {
  #  owner = "rust-lang";
  #  repo = "mdBook";
  #  rev = "v0.4.40";
  #  sha256 = "GGQK2Mf3EK1rwBMzQkAzWAaK6Fh0Qqqf8dtDjZPxOMA=";
  # };
  src = fetchzip {
    url =
      "https://github.com/rust-lang/mdBook/releases/download/v0.4.40/mdbook-v0.4.40-x86_64-unknown-linux-gnu.tar.gz";
    hash = "sha256-ijQbAOvEcmKaoPMe+eZELxY8iCJvrMnk4R07+d5lGtQ=";
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp mdbook $out/bin/
    runHook postInstall
  '';
}
