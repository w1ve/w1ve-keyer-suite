#pragma once

// Starts the HTTP server and the sidetone edge stream. Configuration lives in the page;
// the firmware keeps the keyer engine and the panel as the only things that must always work.
void web_init(void);
