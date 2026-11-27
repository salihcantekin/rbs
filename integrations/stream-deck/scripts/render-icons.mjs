// Renders the plugin icon PNGs (256 and 512) from an SVG with Playwright's Chromium.
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
<rect width="256" height="256" rx="56" fill="#16355f"/>
<circle cx="128" cy="128" r="70" fill="none" stroke="#4c9aff" stroke-width="16"/>
<circle cx="128" cy="128" r="30" fill="#f2f3f5"/></svg>`;
const out = "com.rbs.streamdeck.sdPlugin/imgs/";
const browser = await chromium.launch();
for (const [size, name] of [[256, "plugin.png"], [512, "plugin@2x.png"]]) {
	const page = await browser.newPage({ viewport: { width: size, height: size } });
	await page.setContent(`<body style="margin:0;background:transparent">${svg.replace('width="256" height="256"', `width="${size}" height="${size}"`)}</body>`);
	writeFileSync(out + name, await page.screenshot({ omitBackground: true }));
}
await browser.close();
