{
  inputs,
  pkgs,
  system,
}: let
  packageFor = revision:
    ((import ../flake.nix).outputs (inputs // {self = {shortRev = revision;};}))
    .packages.${system}.default;
  before = packageFor "0000000";
  after = packageFor "1111111";
in
  assert pkgs.lib.assertMsg
    (before.cargoArtifacts.drvPath == after.cargoArtifacts.drvPath)
    "A source revision change must reuse iamb's Cargo dependency artifacts";
  assert pkgs.lib.assertMsg
    (before.version == "0000000" && after.version == "1111111" && before.drvPath != after.drvPath)
    "The iamb package must still track the source revision";
    pkgs.runCommand "iamb-dependency-cache-check" {} ''
      touch "$out"
    ''
