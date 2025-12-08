{ stdenv, fetchzip, }:

stdenv.mkDerivation {
  pname = "mdbook_admonish";
  version = "1.18.0";

  # src = fetchFromGitHub {
  #  owner = "tommilligan";
  #  repo = "mdbook-admonish";
  #  rev = "v1.18.0";
  #  sha256 = "GNQIOjgHCt3XPCzF0RjV9YStI8psLdHhTPuTkdgx8vA=";
  # };
  src = fetchzip {
    url =
      "https://github.com/tommilligan/mdbook-admonish/releases/download/v1.18.0/mdbook-admonish-v1.18.0-x86_64-unknown-linux-gnu.tar.gz";
    hash = "sha256-L7Vt3a1vz1aO4ItCSpKqn+413JGZZ9R+ukqgsE38fMc=";
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp mdbook-admonish $out/bin/
    runHook postInstall
  '';
}
