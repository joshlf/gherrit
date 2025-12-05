# Style Guidelines

This document covers code style and formatting guidelines for the project.

## Formatting

Maintain consistent formatting by using `cargo fmt`.

## Comments

- Wrap all comments (`//`, `///`, `//!`) at **80 columns** from the left margin,
  taking into account any preceding code or comments.
- **Exceptions:** Markdown tables, ASCII diagrams, long URLs, code blocks, or
  other cases where wrapping would impair readability.

## Markdown Files

- Wrap paragraphs and bulleted lists at **80 columns** from the left margin,
  taking into account any preceding code or comments.
- Always put a blank line between a section header and the beginning of the section.

## Pull Requests and Commit Messages

Use GitHub issue syntax in commit messages:

- Resolves issue: `Closes #123`
- Progress on issue: `Makes progress on #123`
