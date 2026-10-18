{
  description = "plain: compile, test and deploy EVM smart contracts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # the platforms plain ships solc support for, minus x86_64-darwin, which nixpkgs 26.11 dropped
      # (Intel Macs use the installer or mise); on aarch64-linux, solc has official builds only from 0.8.31
      systems = [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forEachSystem (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "plain";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false; # the fixture test downloads solc, and the build sandbox has no network
          meta = {
            description = "Compile, test and deploy EVM smart contracts";
            homepage = "https://getplain.org";
            license = pkgs.lib.licenses.mit;
            mainProgram = "plain";
          };
        };
      });
    };
}
