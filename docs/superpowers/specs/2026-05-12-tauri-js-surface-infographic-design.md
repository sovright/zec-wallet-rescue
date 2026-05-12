# Tauri JS Surface Infographic Design

## Goal

Add a security-focused infographic to the ZECK Tauri GUI that explains why the JavaScript side has a small audit surface relative to ZODL.

## Placement

Place the infographic on the welcome screen after the existing three introductory cards. The welcome step is the right location because users see it before entering a seed phrase.

## Claims

Keep the copy narrow and evidence-backed:

- ZECK ships one runtime npm dependency in the GUI: `@tauri-apps/api`.
- ZECK does not use a frontend framework stack: no React, Vue, Svelte, router, state library, UI kit, analytics SDK, or third-party widget code.
- ZODL is a fuller everyday wallet surface, while ZECK is a single-purpose rescue flow.
- The comparison is about JavaScript GUI attack surface, not a broad claim that ZECK is categorically more secure than ZODL.

## Visual Structure

Use HTML and CSS only:

- A title: "Small JavaScript Surface by Design".
- Three compact metric tiles for dependency count, framework absence, and rescue-only scope.
- A two-column comparison strip:
  - ZODL: everyday wallet surface.
  - ZECK: rescue-only surface.

## Implementation Constraints

- Do not add JavaScript.
- Do not add npm dependencies.
- Keep styling consistent with the existing ZECK desktop app.
- Ensure the layout wraps cleanly on narrow screens.

## Verification

- Inspect `gui/package.json` and `gui/package-lock.json` to confirm no dependency changes.
- Run a targeted build or static check if available.
- Confirm the welcome-screen HTML still includes only the existing `main.js` script tag.
