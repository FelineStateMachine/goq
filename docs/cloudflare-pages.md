# Publish goq.sh with Cloudflare Pages

The `website/` directory is a complete static site. GitHub Actions validates it
without a package install or framework build, then deploys it to Cloudflare
Pages with Wrangler. In this release process, a push to `main` that changes the
site or its workflow is the production release.

## One-time dashboard setup

In the Cloudflare dashboard, open **Workers & Pages**, create a Pages project,
and choose **Direct Upload**. Name the project `goq-sh`. The project must exist
before the first production workflow runs; use the current `website/` directory
for the dashboard's required initial upload. GitHub Actions replaces that
deployment after the first merge.

Do not also connect the Cloudflare Git integration: the repository workflow is
the sole publisher, which prevents two independent systems from racing to
deploy the same commit.

In the GitHub repository, the `main` environment must contain these environment
secrets:

- `CLOUDFLARE_ACCOUNT_ID`
- `CLOUDFLARE_API_TOKEN`

The API token needs **Account / Cloudflare Pages / Edit** permission for the
account that owns the project. These secret names are already present in the
repository's `main` environment.

The **Website / Static site** job validates JavaScript, required assets,
internal asset references, and security-header configuration on pull requests.
On a push to `main`, the **Website / Cloudflare Pages** job waits for that check,
enters the `main` environment, and deploys `website/` to the production branch
of the `goq-sh` Pages project.

## Domain and release gate

After the first successful deployment, open the Pages project's **Custom
domains** screen and add `goq.sh`. Complete the DNS prompts in the account that
owns the zone. Add `www.goq.sh` separately only if it should also serve or
redirect to the site.

Protect `main` in GitHub and require these checks before merge:

- **CI / Complete demo gate**
- **Website / Static site** when it is present

The site release path is then:

1. Open a pull request with website changes.
2. Merge only after the repository checks pass.
3. The production job deploys the merged `main` commit through the protected
   GitHub environment.
4. Cloudflare publishes the resulting immutable Pages deployment to `goq.sh`.

This workflow does not publish pull-request previews because environment secrets
are deliberately unavailable to untrusted PR code. Preview deployments can be
added later with a separate least-privilege project and credential if needed.
