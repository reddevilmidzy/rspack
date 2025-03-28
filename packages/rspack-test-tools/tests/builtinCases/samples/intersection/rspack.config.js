/** @type {import("@rspack/core").Configuration} */
module.exports = {
	entry: {
		index: {
			import: ["./index.js"]
		},
		index2: {
			import: ["./index2.js"]
		}
	},
	optimization: {
		removeAvailableModules: true,
		providedExports: true,
		usedExports: "global"
	}
};
