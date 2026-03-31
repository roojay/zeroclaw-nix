{
  description = "ZeroClaw — Nix-focused fork with NixOS module";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          zeroclaw-web = pkgs.callPackage ./nix/web.nix {};

          zeroclaw = pkgs.callPackage ./nix/package.nix {
            zeroclaw-web = self.packages.${system}.zeroclaw-web;
          };

          zeroclaw-desktop = pkgs.callPackage ./nix/desktop.nix {};

          default = self.packages.${system}.zeroclaw;
        });

      nixosModules.default = ./nix/module.nix;
    };
}
