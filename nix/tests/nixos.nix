{
  pkgs,
  octosModule,
}:

pkgs.testers.nixosTest {
  name = "ra-nixos-test";

  nodes.machine = {
    imports = [ octosModule ];
    programs.ra = {
      enable = true;
      # enableExtraPackages = true;  # Skipped to avoid resource bloat (chromium/ffmpeg/libreoffice/etc.) in VM test
      enableAppSkills = true;
      channels = [
        "telegram"
        "discord"
      ];
      service = {
        enable = true;
        port = 50080;
        dataDir = "/var/lib/ra-test";
        authToken = "test-token";
      };
    };
  };

  testScript = ''
    machine.wait_for_unit("ra-serve.service")
    machine.wait_for_open_port(50080)

    # Check if extra packages are installed
    # machine.succeed("chromium --version")
    # machine.succeed("ffmpeg -version")

    # Check if custom data directory was created with correct permissions
    machine.succeed("ls -ld /var/lib/ra-test | grep '^drwxrwx---'")

    # Check if ra version works
    machine.succeed("ra --version")

    # Verify app-skills binaries are installed when enableAppSkills = true
    for bin in ["news_fetch", "deep-search", "deep_crawl", "send_email", "account_manager", "clock", "weather"]:
        machine.succeed(f"command -v {bin}")

    # Verify the service is actually responding on the custom port
    machine.succeed("curl -f http://127.0.0.1:50080")
  '';
}
