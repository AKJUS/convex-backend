<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static-http.s3.amazonaws.com/logo/convex-logo-light.svg" width="600">
  <source media="(prefers-color-scheme: light)" srcset="https://static-http.s3.amazonaws.com/logo/convex-logo.svg" width="600">
  <img alt="Convex logo" src="https://static-http.s3.amazonaws.com/logo/convex-logo.svg" width="600">
</picture>
</p>

The [Convex managed hosted](https://www.convex.dev/plans) product includes a
generous free tier and provides a seamless, reliable, cost-effective platform
that allows you to focus on building your application without worrying about
infrastructure.

That being said, we understand that won't work for everyone. You are welcome to
self-host Convex on your own infrastructure instead.

Community support is available for self-hosted Convex in the `#open-source`
channel in the [Convex Discord](https://discord.gg/convex).

Development of the Convex backend is led by the Convex team. We
[welcome bug fixes](./CONTRIBUTING.md) and
[love receiving feedback](https://discord.gg/convex). We keep this repository
synced with any internal development work within a handful of days.

# Self Hosting Via Docker

You'll need to have [Docker](https://docs.docker.com/desktop/) installed to run
convex in Docker.

```sh
cd self-hosted
# Pull the latest docker images
docker compose pull
# Run the containers
docker compose up
```

Once your Convex backend is running in Docker, you can ask it to generate admin
keys for use from the dashboard/CLI.

```sh
docker exec convex-local-backend ./generate_admin_key.sh
```

Using your admin key, push code to your backend. Admin key should be kept secure
to just the developers who are administering the application on your backend.

Visit the dashboard at `http://localhost:6791`. The backend listens on
`http://127.0.0.1:3210`. The backend's http actions are available at
`http://127.0.0.1:3211`.

In your Convex project, add your url and admin key to a `.env.local` file (which
should not be committed to source control):

```sh
CONVEX_SELF_HOST_URL='http://127.0.0.1:3210'
CONVEX_SELF_HOST_ADMIN_KEY='<your deploy key>'
```

Now you can run commands in your Convex project, to push code, run queries,
import data, etc. To use these commands, you'll need the latest alpha version of
Convex.

```sh
npm install convex@alpha
```

```sh
npx convex self-host dev
npx convex self-host run <run args>
npx convex self-host import <import args>
npx convex self-host --help  # see all available commands
```

By default, the backend will store its data in a volume managed by Docker. Note
that you'll need to set up persistent storage on whatever cloud hosting platform
you choose to run the Docker container on (e.g. AWS EBS). The default database
is SQLite, but for production workloads, we recommend running Convex backed by
Postgres. Follow [these instructions](#self-hosting-on-postgres-with-neon) to
connect to Postgres.

# Self Hosting with [Fly.io](https://fly.io/)

From the self-hosted directory, deploy the backend.

```sh
fly launch
```

Note the URL of the app that gets printed out, which will be of the form
`https://<app-name>.fly.dev` (no trailing slash).

In the fly.toml file, change the env variables `CONVEX_CLOUD_ORIGIN` and
`CONVEX_SITE_ORIGIN` to:

```sh
CONVEX_CLOUD_ORIGIN="<fly app url>"
CONVEX_SITE_ORIGIN="<fly app url>/http"
```

And re-deploy to pick up the changes.

```sh
fly deploy
```

Copy and paste the fly url to set `NEXT_PUBLIC_DEPLOYMENT_URL` in the
dashboard/fly.toml file.

```sh
cd dashboard
fly launch
```

Visit the dashboard at the url output from the fly deploy command. Generate
admin key to login to the dashboard.

```sh
fly ssh console --command "./generate_admin_key.sh"
```

In your frontend app directory

```sh
npm install convex@alpha
```

Write these environment variables to .env.local

```sh
CONVEX_SELF_HOST_URL='<NEXT_PUBLIC_DEPLOYMENT_URL>'
CONVEX_SELF_HOST_ADMIN_KEY='<your-admin-key>'
```

Push your Convex functions

```sh
npx convex self-host dev
```

Visit the dashboard - you should see your functions and be able to edit data,
run functions, etc.

### HTTP Actions

Note that HTTP actions run on your fly app url under the `/http` path. For
example:

- If your fly app is deployed at `https://self-hosted-backend.fly.dev`
- And you have an HTTP action named `/sendEmail`
- You would call it at `https://self-hosted-backend.fly.dev/http/sendEmail`

# Self Hosting on Postgres with [Neon](https://neon.tech)

Create a project on Neon. Copy the connection string from the Neon dashboard.

```sh
export DATABASE_CONNECTION='<connection string>'
```

Create the database

```sh
psql $DATABASE_CONNECTION -c "CREATE DATABASE convex_self_hosted"
```

Strip database name and query params from the connection string. It should end
in neon.tech

```sh
export DATABASE_URL=$(echo $DATABASE_CONNECTION | sed -E 's/\/[^/]+(\?.*)?$//')
```

Update your `DATABASE_URL` environment variable. If you're deploying on
[Fly](https://fly.io):

```sh
fly secrets set DATABASE_URL=$DATABASE_URL
```

After you've deployed with the environment variable set, check that the database
is connected to your self-hosted convex backend. There should be a line like
"Connected to Postgres" in the logs. If you're deploying on
[Fly](https://fly.io):

```sh
fly logs
```

These instructions should work for any Postgres database, not just Neon.

# Settings

- You may opt out of the beacon we use to improve the product by setting the
  environment variable `DISABLE_BEACON` to `true`. Read more about the beacon
  [here](../crates/local_backend/README.md#disclaimers)
- You can turn on log redaction by setting the environment variable
  `REDACT_LOGS_TO_CLIENT` to `true`. This may be useful for hiding PII in
  production deployments. This is turned on automatically in the cloud-hosted
  product.

# Self Hosting Via Running Binary Directly

### Get convex-local-backend Binary

You can either [build from source](../README.md) or use the precompiled
binaries. You can download the latest precompiled binary release from
[Releases](https://github.com/get-convex/convex-backend/releases). If your
platform is not supported, leave us a github issue. In the meantime, you can
build from source.

_Note: On MacOS you might need to hold the `option` key and double click the
binary file in Finder once, to circumvent the
[Gatekeeper](https://support.apple.com/en-us/102445) warning._

### Generate a new instance secret

Instance secret is the secret to the backend. Keep very safe and only accessible
from the backend itself. Generate a new random instance secret with

```sh
cargo run -p keybroker --bin generate_secret
```

It will look like this:
`4361726e697461732c206c69746572616c6c79206d65616e696e6720226c6974`

### Generate a new admin key

With the instance name and instance secret, generate an admin key. Admin key is
required to push code to the backend and take other administrator operations.

```sh
cargo run -p keybroker --bin generate_key -- convex-self-hosted 4361726e697461732c206c69746572616c6c79206d65616e696e6720226c6974
```

It will look like
`convex-self-hosted|01c046ab1512d9306a6abda3eedec5dfe862f1fe0f66a5aee774fb9ae3fda87706facaf682b9d4f9209a05e038cbd6e9b8`

### Run your backend instance

Adjust the path based on where you downloaded the binary to or add it to your
`PATH`. The backend will store its database in the current-working-directory
(not where the binary file lives).

Use the instance name and instance secret to start your backend.

```sh
./convex-local-backend --instance-name convex-self-hosted --instance-secret 4361726e697461732c206c69746572616c6c79206d65616e696e6720226c6974
```

You can run `./convex-local-backend --help` to see other options for things like
changing ports, convex origin url, convex site url, local storage directories
and other configuration.

### Run the dashboard

You can run the dashboard locally with `just rush install` and `npm run dev`
from `npm-packages/dashboard-self-hosted`.

### Use your backend.

Using your admin key, push code to your backend. Admin key should be kept secure
to just the developers who are administering the application on your backend.

```sh
cd your_project
npm install
npx convex dev --url "http://127.0.0.1:3210" --admin-key 'convex-self-hosted|01c046ab1512d9306a6abda3eedec5dfe862f1fe0f66a5aee774fb9ae3fda87706facaf682b9d4f9209a05e038cbd6e9b8'
```

# Upgrading your self-hosted backend on a production instance.

In order to safely migrate to a new version of self-hosted, there are two
options.

## Option 1: Export/Import your database (higher downtime + easy, recommended)

1. Take down external traffic to your backend
2. Export your database with `npx convex export`
3. Save your environment variables with `npx convex env list` (or via
   dashboard).
4. Upgrade the backend docker image (or binary)
5. Import from your backup with `npx convex import --replace-all`
6. Bring back your environment variables with `npx convex env set` (or via
   dashboard)
7. Bring back external traffic to your backend

Given that exports/imports can be expensive if you have a lot of data, this can
incur downtime. You can get a sense of how much downtime safely, by running an
export while your self-hosted instance is up. For smaller instances, this may be
quick and easy.

However to safely avoid losing data, it's important that the final export is
done after load is stopped from your instance, since exports are taken at a
snapshot in time.

## Option 2: Upgrade in-place (lower downtime)

This is a more manual, more fiddly process, but it incurs less downtime. If you
choose to go this route, please be careful, and feel free to reach out for
guidance.

You will need to upgrade through each intermediate binary revision specified via
`git log crates/model/src/migrations.rs`.

Each upgrade will incur a small amount of downtime, but the underlying database
will be upgraded in-place while your app still functions. You need to allow the
backend to run at each intermediate revision until it is ready.

Look for loglines like this - and follow those instructions to complete the
in-place upgrade. Each migration will let you know which logline to wait for to
determine that the in-place upgrade is complete.

```
Executing Migration 114/115. MigrationComplete(115)
```

Please feel free to reach out to us on [Discord](https://convex.dev/community)
if you have any questions.
