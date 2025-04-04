// @ts-check
// Note: type annotations allow type checking and IDEs autocompletion

// eslint-disable-next-line @typescript-eslint/no-var-requires
const lightCodeTheme = require("prism-react-renderer/themes/github");
// eslint-disable-next-line @typescript-eslint/no-var-requires
const darkCodeTheme = require("./src/theme/prism-theme/oneDark");

// Load environment variables.
require("dotenv").config({ path: ".env.local" });

const ENTRY_POINTS_TO_DOCUMENT = [
  "browser",
  "server",
  "react",
  "react-auth0",
  "react-clerk",
  "nextjs",
  "values",
];

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: "Convex Developer Hub",
  tagline: "The source for documentation about Convex.",
  url: "https://docs.convex.dev",
  baseUrl: "/",
  onBrokenLinks: "throw",
  onBrokenMarkdownLinks: "throw",
  favicon: "img/favicon.ico",
  organizationName: "get-convex", // Usually your GitHub org/user name.
  projectName: "Convex", // Usually your repo name.

  // Even if you don't use internalization, you can use this field to set useful
  // metadata like html lang. For example, if your site is Chinese, you may want
  // to replace "en" with "zh-Hans".
  i18n: {
    defaultLocale: "en",
    locales: ["en"],
  },
  customFields: {
    // Make these environment variables available to the docs site.
    NODE_ENV: process.env.NODE_ENV,
    KAPA_AI_PROJECT: process.env.KAPA_AI_PROJECT,
    KAPA_AI_KEY: process.env.KAPA_AI_KEY,
    POST_HOG_KEY: process.env.POST_HOG_KEY,
    POST_HOG_HOST: process.env.POST_HOG_HOST,
  },
  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    {
      // Replace with your project's social card
      // image: "img/docusaurus-social-card.jpg", // TODO!
      docs: {
        sidebar: {
          hideable: false,
          autoCollapseCategories: true,
        },
      },
      navbar: {
        hideOnScroll: true,
        logo: {
          href: "https://convex.dev",
          alt: "Convex",
          src: "img/convex-light.svg",
          srcDark: "img/convex-dark.svg",
        },
        items: [
          {
            type: "docSidebar",
            position: "left",
            // If you change this make sure to update
            // src/theme/DocSidebar/Desktop/index.js
            // home link
            sidebarId: "docs",
            label: "docs",
          },
          {
            type: "custom-convex-search",
            position: "left",
          },
          {
            type: "custom-convex-ai-chat",
            position: "left",
          },
          {
            href: "https://dashboard.convex.dev",
            label: "Dashboard",
            position: "right",
            className: "convex-dashboard-button",
          },
          {
            href: "https://stack.convex.dev/",
            label: "Blog",
            position: "right",
          },
          {
            href: "https://github.com/get-convex",
            label: "GitHub",
            position: "right",
            className: "convex-github-logo convex-icon-link",
          },
          {
            href: "https://convex.dev/community",
            label: "Discord",
            position: "right",
            className: "convex-discord-logo convex-icon-link",
          },
        ],
      },
      footer: {
        links: [
          {
            href: "https://convex.dev/releases",
            label: "Releases",
          },
          {
            label: "GitHub",
            href: "https://github.com/get-convex",
            className: "convex-github-logo convex-icon-link",
          },
          {
            label: "Discord",
            to: "https://convex.dev/community",
            className: "convex-discord-logo convex-icon-link",
          },
          {
            label: "Twitter",
            href: "https://twitter.com/convex_dev",
            className: "convex-twitter-logo convex-icon-link",
          },
        ],
        copyright: `Copyright © ${new Date().getFullYear()} Convex, Inc.`,
      },
      prism: {
        theme: lightCodeTheme,
        darkTheme: darkCodeTheme,
        additionalLanguages: ["rust", "kotlin", "swift"],
      },
      image: "img/social.png",
      metadata: [
        { name: "twitter:card", content: "summary_large_image" },
        { name: "twitter:image:alt", content: "Convex Docs logo" },
        { name: "og:image:alt", content: "Convex Docs logo" },
      ],
    },
  themes: ["mdx-v2"],
  presets: [
    [
      "classic",
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        gtag: {
          trackingID: "G-BE1B7P7T72",
        },
        docs: {
          sidebarPath: require.resolve("./sidebars.js"),
          routeBasePath: "/",
          async sidebarItemsGenerator({
            defaultSidebarItemsGenerator,
            ...args
          }) {
            const originalSidebarItems =
              await defaultSidebarItemsGenerator(args);

            // Remove "API" and "Generated Code" from the main sidebar because
            // they have their own tab.
            if (
              args.item.type === "autogenerated" &&
              args.item.dirName === "."
            ) {
              const finalSidebarItems = originalSidebarItems.filter(
                (item) =>
                  !("label" in item) ||
                  (item.label !== "API" &&
                    item.label !== "Generated Code" &&
                    item.label !== "HTTP API"),
              );
              return finalSidebarItems;
            }

            // Drop `index.md` from "Generated Code" because it's already included
            // as the category index and Docusaurus is dumb and adds it twice.
            if (
              args.item.type === "autogenerated" &&
              args.item.dirName === "generated-api"
            ) {
              return originalSidebarItems.filter(
                (item) => !("id" in item) || item.id !== "generated-api/index",
              );
            }

            // If we have other autogenerated items, don't touch them.
            if (
              args.item.type === "autogenerated" &&
              args.item.dirName !== "api"
            ) {
              return originalSidebarItems;
            }

            /**
             * Custom generator for api sidebar items.
             *
             * We have a custom generator for the items in the sidebar because
             * we reorganize the API docs that docusaurus-plugin-typedoc generates.
             *
             * The original scheme is:
             * - API Reference
             *   - Modules
             *     - One item per entry point
             *   - Interfaces
             *     - All the interfaces for all the entry points
             *   - Classes
             *     - All the classes for all the entry points
             *
             * We reorganize that into:
             * - API Reference
             *   - convex/$entryPoint
             *     - classes, interfaces for $entrypoint
             *   - Generated Code
             *     - generated hooks and types.
             */
            const entryPointToItems = {};
            for (const entryPoint of ENTRY_POINTS_TO_DOCUMENT) {
              entryPointToItems[entryPoint] = [];
            }

            for (const category of originalSidebarItems) {
              // Skip the "Table of contents" category because we don't need
              // it, the "Modules" category because we create that ourselves
              // below, and "Readme" because it's already in sidebars.js.

              // The rest are things like "Classes" and "Interfaces" that we
              // want to reorganize.
              if (
                "items" in category &&
                (!("label" in category) ||
                  (category.label !== "Readme" &&
                    category.label !== "Table of contents" &&
                    category.label !== "modules"))
              ) {
                for (const item of category.items) {
                  if (!("id" in item)) {
                    continue;
                  }
                  // The original item ID looks like "api/classes/browser.ConvexHttpClient"
                  // and we want to extract "browser" because that's the entry point.
                  const pathParts = item.id.split("/");
                  const itemName = pathParts[pathParts.length - 1];
                  // Undo react-auth0 -> react_auth0 normalization.
                  const entryPoint = itemName.split(".")[0].replace("_", "-");
                  if (!ENTRY_POINTS_TO_DOCUMENT.includes(entryPoint)) {
                    throw new Error(
                      "Couldn't sort API reference doc by entry point: " +
                        item.id,
                    );
                  }

                  entryPointToItems[entryPoint].push({
                    ...item,
                    label: itemName.split(".")[1],
                  });
                }
              }
            }

            const entryPointCategories = ENTRY_POINTS_TO_DOCUMENT.map(
              (entryPoint) => {
                // Normalize the same way original sidebar items are.
                const entryPointForId = entryPoint.replace("-", "_");
                const items = entryPointToItems[entryPoint];
                const id = "api/modules/" + entryPointForId;
                const label = "convex/" + entryPoint;

                return items.length === 0
                  ? { type: "doc", id, label }
                  : {
                      type: "category",
                      label,
                      link: { type: "doc", id },
                      items,
                    };
              },
            );
            return entryPointCategories;
          },
        },
        blog: {
          showReadingTime: true,
        },
        theme: {
          customCss: require.resolve("./src/css/custom.css"),
        },
      }),
    ],
  ],
  plugins: [
    [
      "docusaurus-plugin-typedoc",
      {
        id: "api",
        entryPoints: ENTRY_POINTS_TO_DOCUMENT.map(
          (entryPoint) => "../convex/src/" + entryPoint + "/index.ts",
        ),
        tsconfig: "../convex/tsconfig.json",
        excludePrivate: true,
        excludeInternal: true,
        // Don't generate "defined in" text when generating docs because our
        // source isn't public.
        disableSources: false,
        sourceLinkTemplate:
          "https://github.com/get-convex/convex-js/blob/main/{path}#L{line}",
        gitRemote: "https://github.com/get-convex/convex-js",
        basePath: "../convex/src",
        // Keep everything in source order so we can be intentional about our
        // ordering. This seems to only work for functions, variables and type
        // aliases but it's something.
        sort: "source-order",
        out: "api",
        sidebar: {
          // Don't generate sidebar_label so we can always define it ourselves
          autoConfiguration: false,
        },
      },
    ],
    "./src/plugins/metrics",
    "./src/plugins/prefixIds",
    async function tailwindPlugin() {
      return {
        name: "docusaurus-tailwindcss",
        configurePostCss(postcssOptions) {
          postcssOptions.plugins.push(require("tailwindcss"));
          postcssOptions.plugins.push(require("autoprefixer"));
          postcssOptions.plugins.push(require("postcss-nested"));
          return postcssOptions;
        },
      };
    },
  ],
  scripts: [
    {
      src: "https://plausible.io/js/script.js",
      defer: true,
      "data-domain": "docs.convex.dev",
    },
    {
      src: "https://widget.kapa.ai/kapa-widget.bundle.js",
      "data-button-hide": "true",
      "data-modal-override-open-class": "js-launch-kapa-ai",
      "data-website-id": "a20c0988-f33e-452b-9174-5045a58b965d",
      "data-project-name": "Convex",
      "data-project-color": "#141414",
      "data-project-logo":
        "https://img.stackshare.io/service/41143/default_f1d33b63d360437ba28c8ac981dd68d7d2478b22.png",
      "data-user-analytics-fingerprint-enabled": "true",
      async: true,
    },
  ],
  clientModules: [
    require.resolve("./src/components/Analytics/analyticsModule.ts"),
  ],
};

module.exports = config;
