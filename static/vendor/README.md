# Vendored browser dependencies

These are third-party files, vendored so the operator dashboard runs with no network
access and no public CDN. Each remains under its own licence.

| Asset | Version | Licence |
| --- | --- | --- |
| `bootstrap.min.css`, `bootstrap.bundle.min.js` | 5.3.2 | MIT, © Twitter, Inc. and the Bootstrap Authors |
| `bootstrap-icons.min.css`, `fonts/bootstrap-icons.*` | 1.11.3 | MIT, © The Bootstrap Authors |
| `chart.umd.min.js` | 4.4.2 | MIT, © Chart.js Contributors |
| Popper embedded in `bootstrap.bundle.min.js` | 2.11.8 | MIT, © Federico Zivolo |
| `@kurkle/color` embedded in `chart.umd.min.js` | 0.3.2 | MIT, © Jukka Kurkela |
| `fonts.css` and the webfonts it references | — | SIL Open Font License 1.1 (Space Grotesk, IBM Plex Mono) |

`fonts.css` uses Google Fonts declarations with local URLs for the copies in `fonts/`.
The recorded font files match the upstream binaries; Google may change its generated
stylesheet declarations over time.

The Chart.js copy includes a jsDelivr-generated comment followed by the exact published UMD bytes.
Bootstrap, its icons and the font binaries match their recorded upstream copies.

The original copyright and permission notices, including the embedded Popper and colour libraries,
are included in `licenses/`.
`licenses/sources.json` records the upstream locations used for those texts.
For the two embedded libraries, that location is the exact published package
archive containing `LICENSE.md`. `asset-manifest.json` records every distributed
file's size and SHA-256 plus the checked source locations.

To refresh a dependency, download the same path at the new version, check its
embedded libraries and notices, and regenerate the asset manifest after review.
