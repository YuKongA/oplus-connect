{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.oplus-connect;

  # Ports the embedded daemon binds: control/data sockets the device dials back
  # on, plus the SDP announcement listener and its source port.
  tcpPorts = [
    10150
    10152
    10155
    46888
    46999
    47888
    47999
    48999
    48888
  ];
  udpPorts = [
    10150
    10151
  ];
in
{
  options.programs.oplus-connect = {
    enable = lib.mkEnableOption "OPLUS Connect, a native Linux client for OPPO and OnePlus devices";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "oplus-connect.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The oplus-connect package to use.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Open the TCP and UDP ports used for device discovery and streaming.
        Without them a firewall rejects the connection requests the device
        sends back to this machine.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # The GUI runs the daemon in-process, so no additional service is needed.

    # Screen sharing is carried over PipeWire and authorized through the
    # RemoteDesktop, ScreenCast and InputCapture portals.
    services.pipewire.enable = lib.mkDefault true;
    xdg.portal.enable = lib.mkDefault true;

    # The GPUI renderer draws through Vulkan.
    hardware.graphics.enable = lib.mkDefault true;

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall tcpPorts;
    networking.firewall.allowedUDPPorts = lib.mkIf cfg.openFirewall udpPorts;
  };
}
