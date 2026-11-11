/**
 * Two build modes, selected by `MEM0_DASHBOARD_OUTPUT`:
 *
 * - unset / "standalone" (default) — the Docker image: `node server.js` serves the app and also
 *   runs middleware and the `/api/*` route handlers.
 * - "export" — the desktop app: `next build` emits plain HTML/CSS/JS into `out/`, which the Python
 *   backend serves from the same origin as the REST API. Static export supports neither
 *   middleware nor route handlers, so those files do not exist in this tree; the session cookie
 *   lives in the backend (`server/routers/session.py`) and the route guard runs on the client.
 *
 * @type {import('next').NextConfig}
 */
const staticExport = process.env.MEM0_DASHBOARD_OUTPUT === "export";

const nextConfig = {
  output: staticExport ? "export" : "standalone",
  // Exporting a route to `<route>/index.html` keeps the static tree navigable by a plain file
  // server and matches the fallback rules in server/dashboard_static.py.
  trailingSlash: staticExport ? true : undefined,
  eslint: {
    ignoreDuringBuilds: false,
  },
  typescript: {
    ignoreBuildErrors: false,
  },
  experimental: {
    optimizePackageImports: ["@/components", "@/lib", "@/utils"],
  },
  compress: true,
  images: staticExport
    ? // No image optimizer exists in a static export; `next/image` must pass through.
      { unoptimized: true }
    : {
        formats: ["image/webp", "image/avif"],
      },
  // Response headers and redirects are server features. The backend reproduces both for the
  // desktop build: security headers in server/dashboard_static.py and /settings -> /dashboard/settings.
  headers: staticExport
    ? undefined
    : async () => {
        return [
          {
            source: "/(.*)",
            headers: [
              { key: "X-Frame-Options", value: "DENY" },
              {
                key: "Content-Security-Policy",
                value: "frame-ancestors 'none'",
              },
              { key: "X-Content-Type-Options", value: "nosniff" },
              {
                key: "Referrer-Policy",
                value: "strict-origin-when-cross-origin",
              },
            ],
          },
        ];
      },
  redirects: staticExport
    ? undefined
    : async () => {
        return [
          {
            source: "/settings",
            destination: "/dashboard/settings",
            permanent: false,
          },
        ];
      },
};

export default nextConfig;
