{
  pkgs,
  lib,
  enableAllFeatures ? false,
  enableAppSkills ? false,
  features ? null,
}:

let
  ra-cli = pkgs.callPackage ./cli.nix { features = actualFeatures; };
  ra-app-skills = pkgs.callPackage ./app-skills.nix { };

  actualFeatures =
    if features != null then
      features
    else if enableAllFeatures then
      ra-cli.supportedFeatures
    else
      [ ];
in

pkgs.buildEnv {
  name = "ra";
  paths = [ ra-cli ] ++ lib.optionals enableAppSkills [ ra-app-skills ];

  passthru = {
    inherit ra-cli ra-app-skills;
  };

  meta = with lib; {
    description = "ra - Agentic OS";
    homepage = "https://github.com/icehomura/ra";
    license = licenses.asl20;
    maintainers = [ ];
    platforms = platforms.linux ++ platforms.darwin;
  };
}
