import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  async rewrites() {
    return [
      {
        source: "/install.sh",
        destination:
          "https://raw.githubusercontent.com/emilianscheel/signal-cli/main/install.sh",
      },
    ];
  },
};

export default nextConfig;
