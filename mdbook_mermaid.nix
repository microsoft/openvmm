{ stdenv, fetchzip,
#fetchFromGitHub
}:

stdenv.mkDerivation {
  pname = "mdbook-mermaid";
  version = "0.14.0";

  # src = fetchFromGitHub {
  #  owner = "badboy";
  #  repo = "mdbook-mermaid";
  #  rev = "v0.14.0";
  #  sha256 = "elDKxtGMLka9Ss5CNnzw32ndxTUliNUgPXp7e4KUmBo=";
  # };
  src = fetchzip {
    url =
      "https://github.com/badboy/mdbook-mermaid/releases/download/v0.14.0/mdbook-mermaid-v0.14.0-x86_64-unknown-linux-gnu.tar.gz";
    hash = "sha256-cbcPoLQ4b8cQ2xk0YnapC9L0Rayt0bblGXVfCzJLiGA=";
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp mdbook-mermaid $out/bin/
    runHook postInstall
  '';
}
