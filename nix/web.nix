{ lib
, buildNpmPackage
, gnused
, pwaOverlay ? null
}:

buildNpmPackage {
  pname = "zeroclaw-web";
  version = "0.6.5";
  src = ./..;
  sourceRoot = "source/web";

  npmDepsHash = "sha256-RMiFoPj4cbUYONURsCp4FrNuy9bR1eRWqgAnACrVXsI=";

  postPatch = lib.optionalString (pwaOverlay != null) ''
    cp ${pwaOverlay}/manifest.json public/manifest.json
    cp ${pwaOverlay}/service-worker.js public/service-worker.js
    mkdir -p public/icons
    cp ${pwaOverlay}/icons/icon-192x192.png public/icons/icon-192x192.png
    cp ${pwaOverlay}/icons/icon-512x512.png public/icons/icon-512x512.png

    ${gnused}/bin/sed -i '/<link rel="icon"/i\    <meta name="theme-color" content="#22d3ee" />\n    <meta name="apple-mobile-web-app-capable" content="yes" />\n    <meta name="apple-mobile-web-app-status-bar-style" content="black-translucent" />\n    <link rel="manifest" href="/_app/manifest.json" />\n    <link rel="apple-touch-icon" href="/_app/icons/icon-192x192.png" />' index.html

    cp ${pwaOverlay}/sw-register.ts src/sw-register.ts
    ${gnused}/bin/sed -i "1s|^|import { registerServiceWorker } from './sw-register';\n|" src/main.tsx
    echo "registerServiceWorker();" >> src/main.tsx
  '';

  installPhase = ''
    runHook preInstall
    cp -r dist $out
    runHook postInstall
  '';

  meta = {
    description = "ZeroClaw web frontend";
    homepage = "https://github.com/kcalvelli/zeroclaw-nix";
    license = with lib.licenses; [ mit asl20 ];
  };
}
