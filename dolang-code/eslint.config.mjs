export default [
    {
        files: ["**/*.ts"],
        ignores: ["out/**", "dist/**", "node_modules/**"]
    },
    {
        plugins: {
            "@typescript-eslint": await import("@typescript-eslint/eslint-plugin")
        },

        languageOptions: {
            parser: await import("@typescript-eslint/parser"),
            ecmaVersion: 2022,
            sourceType: "module"
        },

        rules: {
            curly: "warn",
            eqeqeq: ["warn", "always"],
            "no-throw-literal": "warn",
            semi: ["warn", "always"],
            "no-console": "warn",
            "no-empty": ["warn", { allowEmptyCatch: true }],
            "no-unused-vars": ["warn", { argsIgnorePattern: "^_" }],
            "prefer-const": "warn"
        }
    }
];
