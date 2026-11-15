# Apple Music web UI: what the public bundle gives a native port (2026-11-08)

Fetched anonymously (no account, no cookies, a plain browser User-Agent) from https://music.apple.com/us/browse:
`index~cc245af156.js` (3.3 MB) and `index~83a3c7ea96.css` (870 KB). Nothing here touches the user's
session; the files themselves stay out of the repo (Apple's copyrighted assets), only the measured
design data is kept: `tokens.json` = every `:root` custom property (**574 light, 320 dark
overrides**).

## What is in there

* **Design tokens** (`:root` custom properties): the full system colour set in light and dark
  (`--systemRed … --systemGray`, each with `-vibrant`, `-onDark`, `_IC` increased-contrast variants),
  the label hierarchy (`--systemPrimary … --systemQuinary`, `-onDark`), page backgrounds, the key colour,
  dividers, glass material (`--glassMaterialBackground`, `--glassMaterialBackdropFilter:
  saturate(220%) blur(16px)`), z-layers, transitions, and the whole **type ramp** as `font` shorthands
  (`--large-title`, `--title-1/2/3`, `--headline`, `--body` 13 px /1.23, `--callout`, `--subhead`,
  `--footnote`, `--caption-1/2`, each with `-emphasized`, `-reduced`, `-tall`, `-short` variants).
* **Layout constants**: `--web-navigation-width: 260px` (≥ 767 px), `--bodyGutter: 25px → 40px`
  (≥ 1000 px); the player pill `.chrome-player`: `height 56px; max-width 668px; grid auto 1fr auto;
  border-radius 1000px; padding 0 16px`, glass with `box-shadow 0 10px 40px`; `.playback-controls`:
  progress track 7 px, thumb 18 px, skip buttons 50 px (44 px compact); LCD text `--title-2-emphasized`
  / `--title-2`; navigation list padding-top 23 px; shelf arrows 28×64 px, radius 6 px.
* **141 component families** (Svelte BEM blocks): `navigation`, `navigation-item`, `chrome-player`,
  `player-lcd`, `lcd-meta`, `playback-controls`, `chrome-volume`, `volume-control`, `shelf-grid`,
  `shelf-grid-nav`, `product-lockup`, `horizontal-lockup`, `track-lockup`, `primary-actions`,
  `headings`, `artist-detail-header`, `artist-featured-release/-editorial/-video`, `tracklist-footer`,
  `lyrics`, `up-next-queue`, `mini-player`, `search-input`, `search-scope-bar`, `top-search-lockup`,
  `filter-pill`, `dropdown`, `toggle-button`, `favorite-icon`, `empty-state`, `pulse-spinner`, …
  Each block's CSS gives exact sizes, paddings, radii, fonts, hover/pressed colours and transitions.
* **Icons**: Apple's SVG glyphs are in the bundle but are Apple's assets: the port keeps drawing its own
  equivalents (same metrics, our paths).
* **Fonts**: the web app asks for `-apple-system, BlinkMacSystemFont, SF Pro, Helvetica Neue, Helvetica,
  Arial`; on Linux it renders in the fallback (Helvetica/Arial → Liberation Sans / the system sans).
  SF Pro is licensed for Apple platforms only, so "accurate" on Linux means the same fallback metrics
  (we use Inter today; Liberation Sans matches what Chrome on Fedora shows for music.apple.com).

## Key values

| token | light | dark |
|---|---|---|
| `--keyColor` | `#000000d9` | `#ffffffd9` |
| `--keyColor-rgb` | `0,0,0` | `255,255,255` |
| `--pageBG` | `#fff` | `#1f1f1f` |
| `--pageBG-dark` | `#1f1f1f` | `#1f1f1f` |
| `--systemPrimary` | `#000000d9` | `#ffffffd9` |
| `--systemSecondary` | `#00000080` | `#ffffff8c` |
| `--systemTertiary` | `#00000040` | `#ffffff40` |
| `--systemQuaternary` | `#0000001a` | `#ffffff1a` |
| `--systemQuinary` | `#0000000d` | `#ffffff0d` |
| `--labelDivider` | `#00000026` | `#ffffff1a` |
| `--systemPrimary-onDark` | `#ffffffd9` | `#ffffffd9` |
| `--systemSecondary-onDark` | `#ffffff8c` | `#ffffff8c` |
| `--glassMaterialBackground` | `#f5f5f78c` | `#26262899` |
| `--systemRed` | `#ff3b30` | `#ff453a` |
| `--systemBlue` | `#007aff` | `#0a84ff` |
| `--systemGray` | `#8e8e93` | `#98989d` |

Layout: `--z-unset: unset`, `--z-default: 1`, `--z-bubbles: 50`, `--z-gpu: 1001`, `--z-web-chrome: 9901`, `--z-contextual-menus: 9951`, `--z-modal: 10001`, `--global-transition-property: opacity`, `--global-transition-duration: .1s`, `--global-transition-timing-function: ease-in`, `--global-transition: var(--global-transition-property,opacity) var(--global-transition-duration,.1s) var(--global-transition-timing-function,ease-in)`, `--web-navigation-width: 164px`

Responsive root overrides: `@media (min-width:415px){:root{--viewport-grid-layout:"grid-layout:xs-2"}}` `@media (min-width:484px){:root{--viewport-sidebar:"sidebar:visible";--viewport-grid-layout:"grid-layout:xs-1-sidebar"}}` `@media (min-width:607px){:root{--viewport-search-landing-grid:"search-landing-grid:2"}}` `@media (min-width:484px){:root{--viewport-sidebar:"sidebar:constrained-visible"}}`

Type ramp (light root): `--headline`, `--headline-reduced`, `--intro`, `--callout`, `--body`, `--body-reduced`, `--large-title`, `--title-1`, `--title-2`, `--title-3`, `--subhead`, `--footnote`, `--caption-1`, `--caption-2`, `--large-title-emphasized`, `--title-1-emphasized`, `--title-2-emphasized`, `--title-3-emphasized`, `--headline-emphasized`, `--body-emphasized`, `--callout-emphasized`, `--subhead-emphasized`, `--footnote-emphasized`, `--caption-1-emphasized`, `--caption-2-emphasized`, `--large-title-tall`, `--title-1-tall`, `--title-2-tall`, `--title-3-tall`, `--headline-tall`, `--body-tall`, `--callout-tall`, `--subhead-tall`, `--footnote-tall`, `--caption-1-tall`, `--caption-2-tall`, `--large-title-short`, `--title-1-short`, `--title-2-short`, `--title-3-short`

Measured rules:
* `.chrome-player.svelte-3m7vi0` → `border-radius:1000px;display:grid;grid-template-columns:auto 1fr auto;height:56px;max-width:668px;padding-left:16px;padding-right:16px;place-items:center;position:relative`
* `.navigation.svelte-1ek6j5s` → `display:flex;flex-direction:column;width:100%;z-index:var(--z-web-chrome)`
* `.chrome-player.svelte-3m7vi0:before` → `-webkit-backdrop-filter:var(--glassMaterialBackdropFilter,saturate(220%) blur(16px));backdrop-filter:var(--glassMaterialBackdropFilter,saturate(220%) blur(16px));background:var(--glassMaterialBackground);border-radius:inherit;box-shadow:0 10px 40px var(--glassMaterialShadowColor);content:""`
* `.navigation__scrollable-container.svelte-1ek6j5s` → `overflow-y:auto;scroll-behavior:smooth`
* `.shelf-grid-nav__arrow.svelte-16g850i` → `align-items:center;border:none;border-radius:6px;cursor:pointer;display:flex;height:64px;justify-content:center;overflow:hidden;position:absolute;top:50%;transition:background-color .25s linear,color .25s linear,opacity .25s linear;translate:0 -50%;width:28px;z-index:var(--z-default)`

## How a faithful native port would go (proposal, not started)

1. **Token sheet → Theme.qml**: generate the QML singleton from `tokens.json` (colours light/dark, type
   ramp as pixel sizes/weights/line heights, radii, gutters, glass parameters) so every component
   references Apple's names (`systemPrimary`, `keyColor`, `title2Emphasized`, …).
2. **Chrome**: sidebar 260 px with Apple's item metrics and section headers; the top player pill
   (56 px, 668 px max, glass, LCD with artwork + two title-2 lines + 7 px progress) in the header, the
   volume/lyrics/queue cluster, account/search placement; window background and page gutters (40 px).
3. **Pages, component by component**, each against the measured CSS: shelf grid (card sizes, 6 px
   radii, hover scrim), product lockups (album/playlist cards), horizontal lockups (songs in Home
   shelves), track lists (row height, number column, duration, hover/selection colours, footer), detail
   headers (artwork, headings, primary actions pill buttons, description truncation), artist
   pages (featured release/editorial/video blocks, round header), search (scope bar, top result
   lockup), lyrics (line font, blur), queue panel, Now Playing (full-screen player), empty states.
4. **Reference screenshots** for side-by-side checks: public pages (browse, an album, a playlist, an
   artist) rendered headlessly from the anonymous session, and the signed-in pages the user
   screenshots themselves.

Scale: roughly the size of the original UI build (two to three focused sessions), no account
automation anywhere; the only Apple data used is the public CSS/JS already fetched.
