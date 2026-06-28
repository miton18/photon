# Reste à faire — checklist (sortie = toutes cochées)

## Nettoyage
- [x] 1. Supprimer les branches de fallback in-memory des handlers (PG-only, DATABASE_URL obligatoire) — 144 tests doivent rester verts
- [x] 2. Supprimer les 8 warnings "unused CSS" dans `AdminConsole.svelte`

## Flags persistés à faire appliquer
- [x] 3. Appliquer le flag `transcode` (gater le transcodage vidéo)
- [x] 4. Appliquer le flag `public_signup` (inscription publique gated)
- [x] 5. Appliquer le flag `public_links` (partage d'album par lien public gated)
- [x] 6. Appliquer le flag `require_2fa` (2FA TOTP réel)
- [x] 7. Vrai login web OIDC (remplacer le stub "Continue with OpenID")

## Perf
- [x] 8. `auth_middleware` : authz par requêtes ciblées (plus de snapshot complet par requête)
- [x] 9. Endpoints blob (render/original) : lectures single-row ciblées (plus de snapshot complet)

## Design System
- [x] 10. Écrans restants : Feed/Lightbox/Editor/UploadPanel **déjà en parité** (styles + composants livrés). Reste à porter **Explore** (vue découverte : People/Places/Things/Moments/Media), aujourd'hui un simple stub.

---
Condition de sortie : **toutes les cases ci-dessus cochées**, build + tests verts.
