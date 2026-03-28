# No External Image URLs

Code changes should not reference external image URLs (wikimedia, imgur, etc.) in content files. Images should be hosted locally in /public/images/ or via Vercel blob storage to prevent 404s from external hosts.

## Checklist

- [ ] No new `src=` values pointing to wikimedia.org, imgur.com, or other external image hosts
- [ ] New images are stored in /public/images/ or use Vercel blob storage URLs (hebbkx1anhila5yf.public.blob.vercel-storage.com)
- [ ] Any existing external image URLs referenced in changed files are flagged for migration

## Examples of Violations

### External Wikimedia image (HIGH severity)
```
src: "https://upload.wikimedia.org/wikipedia/commons/..."
```
HIGH: external host, may 404 on deploy — use /public/images/ instead

### External Imgur image (HIGH severity)
```
src: "https://imgur.com/abc123.png"
```
HIGH: external host, unreliable — use /public/images/ or Vercel blob storage instead
