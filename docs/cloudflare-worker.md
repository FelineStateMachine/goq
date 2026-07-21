# Publish goq.sh with a Cloudflare Worker

The `website/` directory is a complete static site. GitHub Actions validates it
without a package install or framework build, then deploys it as static assets
on the existing `goq-sh` Cloudflare Worker with Wrangler. A push to `main` that
changes the site, its Wrangler configuration, or its workflow is a production
release.

## One-time dashboard setup

The `goq-sh` Worker must exist in the Cloudflare account used by CI. Attach
`goq.sh` as its custom domain in **Workers & Pages > goq-sh > Settings >
Domains & Routes**.

GitHub Actions is the sole production publisher. If the Worker was created by
importing this GitHub repository, disable automatic production and preview
deployments under the Worker's **Settings > Build** page so Cloudflare Workers
Builds does not race the repository workflow.

In the GitHub repository, the `main` environment must contain these environment
secrets:

- `CLOUDFLARE_ACCOUNT_ID`
- `CLOUDFLARE_API_TOKEN`

Create the token from Cloudflare's **Edit Cloudflare Workers** template and
scope it to only the account that owns `goq-sh`. The token must be able to edit
Workers Scripts. Never commit the token or account ID.

The **Website / Static site** job validates JavaScript, required assets,
internal asset references, security headers, the fail-closed installer, and the
assets-only Wrangler configuration. On a push to `main`, the **Website /
Cloudflare Worker** job waits for that check, enters the protected `main`
environment, and runs `wrangler deploy` against `wrangler.jsonc`.

The configuration deliberately has no Worker script or assets binding. Requests
are served directly from Cloudflare's static-asset layer, and `website/_headers`
supplies the site's security headers and installer content type.

## Release gate

Protect `main` in GitHub and require these checks before merge:

- **CI / Complete demo gate**
- **Website / Static site** when it is present

The site release path is then:

1. Open a pull request with website or deployment changes.
2. Merge only after the repository checks pass.
3. The production job deploys the merged `main` commit through the protected
   GitHub environment.
4. Cloudflare promotes the immutable Worker version and assets to `goq.sh`.

This workflow does not publish pull-request previews because environment secrets
are deliberately unavailable to untrusted PR code. Preview versions can be
added later with a separate least-privilege credential and explicit promotion
policy.
