{
  description = "Generic Serial Transport MCP Server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        
        # Define dependencies
        pythonPackages = ps: with ps; [
          fastmcp
          pyserial
          pytest
        ];
        
        pythonEnv = pkgs.python3.withPackages pythonPackages;
        rustClient = pkgs.rustPlatform.buildRustPackage {
          pname = "serial-mcp-rust-client";
          version = "0.3.0";
          src = ./rust-client;
          cargoLock.lockFile = ./rust-client/Cargo.lock;
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [ pythonEnv pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy ];
        };

        packages.default = pkgs.stdenv.mkDerivation {
          name = "serial-mcp";
          src = ./.;
          buildInputs = [ pythonEnv pkgs.makeWrapper ];
          installPhase = ''
            mkdir -p $out/bin $out/share/serial-mcp
            # Modules live beside the launchers so the legacy MCP server and
            # Python daemon (which uses serial_mcp_engine, not server.py) resolve them.
            cp server.py serial_mcp_engine.py serial_mcp_ipc.py serial_mcp_daemon.py serial_mcp_terminal.py $out/share/serial-mcp/
            # The compiled rmcp frontend is ready-built at installation; runtime launch never invokes cargo.
            cp ${rustClient}/bin/serial-mcp-rust-client $out/bin/serial-mcp
            cp ${rustClient}/bin/serial-mcp-terminal $out/bin/serial-mcp-terminal
            wrapProgram $out/bin/serial-mcp-terminal \
              --prefix PATH : $out/bin:${pythonEnv}/bin:${pkgs.psmisc}/bin \
              --prefix PYTHONPATH : $out/share/serial-mcp
            chmod +x $out/bin/serial-mcp
            # serial-mcp-legacy is the previous in-process server (one device per MCP process).
            cp server.py $out/bin/serial-mcp-legacy
            chmod +x $out/bin/serial-mcp-legacy

            # The native frontend needs the Python daemon on PATH/PYTHONPATH only when starting it.
            wrapProgram $out/bin/serial-mcp \
              --prefix PATH : ${pythonEnv}/bin:${pkgs.psmisc}/bin \
              --prefix PYTHONPATH : $out/share/serial-mcp
            wrapProgram $out/bin/serial-mcp-legacy \
              --prefix PATH : ${pythonEnv}/bin:${pkgs.psmisc}/bin \
              --prefix PYTHONPATH : $out/share/serial-mcp

            makeWrapper ${pythonEnv}/bin/python3 $out/bin/serial-mcp-daemon \
              --add-flags "-m serial_mcp_daemon" \
              --prefix PATH : ${pkgs.psmisc}/bin \
              --prefix PYTHONPATH : $out/share/serial-mcp

          '';
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/serial-mcp";
        };

        apps.daemon = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/serial-mcp-daemon";
        };

        apps.terminal = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/serial-mcp-terminal";
        };
      }
    );
}
