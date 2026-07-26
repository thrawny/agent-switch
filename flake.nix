{
  description = "agent-switch: session tracker and switcher for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { nixpkgs, ... }:
    let
      forEachSystem = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
    in
    {
      devShells = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              lib.optionals stdenv.isLinux [
                pkg-config
                gtk4
                gtk4-layer-shell
                glib
                cairo
                pango
                gdk-pixbuf
                graphene
                harfbuzz
              ];
          };
        }
      );
    };
}
