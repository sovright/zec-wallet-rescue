# ZECK Static Site

This folder contains the static product splash page and user guide for the ZECK
rescue wallet subdomain.

## Files

- `index.html`: product splash page
- `guide.html`: user guide
- `styles.css`: shared site styles
- `assets/zeck-icon.png`: app icon copied from the Tauri bundle
- `assets/zeck-og.png`: 1200x630 Open Graph and Twitter Card preview image
- `assets/zeck-og.svg`: editable source for the social preview image
- `CNAME`: GitHub Pages custom-domain hint for `rescue.sovright.com`

## Hosting

The site is static and can be served from any CDN or static host. For GitHub
Pages, publish the `site/` directory and keep `CNAME` set to the target
subdomain. For Cloudflare Pages, Netlify, or Vercel, set the publish directory
to `site` and configure the DNS record for `rescue.sovright.com`.

Update the download links in `index.html` once release assets are signed and
published.
