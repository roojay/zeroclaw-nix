{ lib
, rustPlatform
, pkg-config
, openssl
, systemd
, zeroclaw-web ? null
}:

rustPlatform.buildRustPackage {
  pname = "zeroclaw";
  version = "0.6.5";
  src = ./..;

  cargoHash = "sha256-1/s2ijYqanhHIsYSw85c4H3T5phnAfvV7oQeAl/6lxQ=";

  postPatch = lib.optionalString (zeroclaw-web != null) ''
    rm -rf web/dist
    ln -s ${zeroclaw-web} web/dist
  '';

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl systemd ];

  doCheck = false;

  meta = {
    description = "Zero overhead AI assistant";
    homepage = "https://github.com/kcalvelli/zeroclaw-nix";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "zeroclaw";
  };
}
