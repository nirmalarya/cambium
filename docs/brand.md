# Brand: Cambium

**Name: Cambium**. Open source (MIT). In a tree, the cambium is the growth layer; in grafting, it is where the graft fuses with its rootstock.
The earlier misspelling is on the sweep avoid list (brand.json).
Binaries: `cmb` (client) and `cmbd` (server). Static token prefix: `cmbt_`. Cache directory: `~/.cache/cambium`.

## Palette (forest green)
| role | hex |
|---|---|
| accent | `#1d5c45` |
| text | `#16201b` |
| surface | `#f3f6f4` |
| danger | `#b42318` |
| success | `#1d6b3a` |

Source: `design/tokens.json`. The contrast check covered 11 pairs with 0 AA failures.

## Logo
Files in `docs/brand/logo/`: `cambium.svg` (primary, light backgrounds), `cambium-on-dark.svg`
(dark backgrounds, e.g. the UI header), `favicon.svg` (simplified for 16-32 px). A trunk
cross-section: bark, the bright cambium growth layer, then the history rings.

## Logo brief (original)
- **Idea:** growth rings. Concentric rings with one bright outer ring, so it reads as history that only ever grows outward.
- **Favicon:** the monogram `cmb`.
- **Sizes:** it must work at both 16px and 1024px.
- **Deliverables:** SVG, a 1024px icon, a favicon set, and a social preview image.
- **Avoid:** a starburst or asterisk mark, clay orange, or a serif wordmark.

## Voice: plain (not curt), exact (not pedantic), calm (not chirpy)
- Write "commit landed as 3fa1…", not "Awesome! Your commit is live 🎉".
- Every error says what to do next. For example: "forbidden: write on pay/x.rs" plus a hint to ask a repo admin.
- Use numbers. Don't promise scale figures we haven't measured.
- Don't name other products in UI text.
