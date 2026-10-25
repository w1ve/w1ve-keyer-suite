// Splash screen shown briefly at startup: the hero artwork with the app name and version in the
// top-left corner. The name uses the same colour as the "W1VE Keyer Suite" text in the artwork.
export const APP_NAME = "RWK Station";
export const APP_VERSION = "0.1.0";

export default function SplashScreen() {
  return (
    <div className="splash">
      <div className="splash-frame">
        <img className="splash-hero" src="/hero.png" alt="" />
        <div className="splash-title">
          <span className="splash-name">{APP_NAME}</span>
          <span className="splash-version">v{APP_VERSION}</span>
        </div>
      </div>
    </div>
  );
}
