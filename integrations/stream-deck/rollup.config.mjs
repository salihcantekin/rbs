import commonjs from "@rollup/plugin-commonjs";
import json from "@rollup/plugin-json";
import nodeResolve from "@rollup/plugin-node-resolve";
import typescript from "@rollup/plugin-typescript";

/** Bundles the plugin's backend (Node.js) into the .sdPlugin folder. */
export default {
	input: "src/plugin.ts",
	output: {
		file: "com.rbs.streamdeck.sdPlugin/bin/plugin.js",
		format: "esm",
		sourcemap: false,
	},
	external: [/^node:/],
	plugins: [
		typescript({ tsconfig: "./tsconfig.json", include: ["src/**/*.ts"], compilerOptions: { outDir: "com.rbs.streamdeck.sdPlugin/bin" } }),
		nodeResolve({ browser: false, exportConditions: ["node"], preferBuiltins: true }),
		commonjs(),
		json(),
	],
};
