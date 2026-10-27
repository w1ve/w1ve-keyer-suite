import { APP_NAME, APP_VERSION } from "./app-info.js";

// Help > About dialog: the about artwork as the background, with the product name and version in
// the blank band at the top of the artwork and the license lines in the band below it. Clicking
// anywhere on the dialog closes it.
export default function AboutDialog({ onClose }) {
  return (
    <div className="about-backdrop" onClick={onClose}>
      <div
        className="about-box"
        role="dialog"
        aria-label={`About ${APP_NAME}`}
        style={{ backgroundImage: "url(/about.png)" }}
      >
        <div className="about-head">
          <div className="about-name">W1VE Remote Keyer/Router - Station</div>
          <div className="about-version">Version {APP_VERSION}</div>
        </div>
        <div className="about-lines">
          <div>Copyright © 2026 by Downeast Skunkworks &amp; Gerry Hull, W1VE</div>
          <div>MIT Licensed</div>
          <div>Repository: https://github.com/w1ve/w1ve-keyer-suite</div>
        </div>
        <div className="about-close">Click to close</div>
      </div>
    </div>
  );
}
