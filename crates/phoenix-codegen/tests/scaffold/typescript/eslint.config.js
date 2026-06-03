// Flat ESLint config (ESLint 9) for the Phoenix Gen TypeScript scaffold.
//
// Uses typescript-eslint's strict + strict-type-checked rulesets (roadmap §5:
// "eslint + @typescript-eslint strict"). Type-checked rules require a TS program,
// so `parserOptions.projectService` is enabled to pick up tsconfig.json.
import tseslint from "typescript-eslint";

export default tseslint.config(
  {
    files: ["generated/**/*.ts"],
    extends: [
      ...tseslint.configs.strict,
      ...tseslint.configs.strictTypeChecked,
    ],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    // Don't lint the config file itself.
    ignores: ["eslint.config.js"],
  },
);
