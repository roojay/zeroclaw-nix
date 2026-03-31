{ lib
, rustPlatform
, pkg-config
, wrapGAppsHook3
, openssl
, webkitgtk_4_1
, gtk3
, libsoup_3
, glib-networking
, librsvg
, gdk-pixbuf
, cairo
, pango
, libayatana-appindicator
}:

rustPlatform.buildRustPackage {
  pname = "zeroclaw-desktop";
  version = "0.6.5";
  src = ./..;

  cargoHash = "sha256-1/s2ijYqanhHIsYSw85c4H3T5phnAfvV7oQeAl/6lxQ=";

  cargoBuildFlags = [ "-p" "zeroclaw-desktop" ];
  cargoTestFlags = [ "-p" "zeroclaw-desktop" ];

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook3
  ];

  buildInputs = [
    openssl
    webkitgtk_4_1
    gtk3
    libsoup_3
    glib-networking
    librsvg
    gdk-pixbuf
    cairo
    pango
    libayatana-appindicator
  ];

  doCheck = false;

  preFixup = ''
    gappsWrapperArgs+=(--prefix LD_LIBRARY_PATH : "${libayatana-appindicator}/lib")
  '';

  postInstall = ''
    for size in 32 128; do
      mkdir -p $out/share/icons/hicolor/''${size}x''${size}/apps
      cp apps/tauri/icons/''${size}x''${size}.png \
        $out/share/icons/hicolor/''${size}x''${size}/apps/zeroclaw.png
    done
    mkdir -p $out/share/icons/hicolor/scalable/apps
    cp apps/tauri/icons/icon.svg $out/share/icons/hicolor/scalable/apps/zeroclaw.svg

    mkdir -p $out/share/applications $out/etc/xdg/autostart
    cat > $out/share/applications/zeroclaw-desktop.desktop <<EOF
    [Desktop Entry]
    Name=ZeroClaw
    Comment=ZeroClaw Desktop Agent
    Exec=$out/bin/zeroclaw-desktop
    Icon=zeroclaw
    Type=Application
    Categories=Utility;
    StartupNotify=false
    EOF
    cp $out/share/applications/zeroclaw-desktop.desktop $out/etc/xdg/autostart/
  '';

  meta = {
    description = "ZeroClaw desktop tray app (Tauri)";
    homepage = "https://github.com/kcalvelli/zeroclaw-nix";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "zeroclaw-desktop";
  };
}
