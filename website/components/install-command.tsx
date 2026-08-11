"use client";

import { Check, Copy } from "lucide-react";
import { useState } from "react";

const INSTALL_COMMAND =
  "curl -fsSL https://signal-cli.vercel.app/install.sh | sh";

export function InstallCommand() {
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(INSTALL_COMMAND);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      setCopied(false);
    }
  }

  return (
    <button
      type="button"
      onClick={handleCopy}
      aria-label={copied ? "Copied" : "Copy install command"}
      className="group relative inline-flex max-w-full cursor-pointer items-center gap-2.5 overflow-hidden rounded-sm border border-primary bg-[#fdf5e6] px-3 py-2 text-left transition-opacity hover:opacity-95 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-transparent"
    >
      <span
        aria-hidden
        className="pointer-events-none absolute inset-0 opacity-[0.07]"
        style={{
          backgroundImage:
            "url(\"data:image/svg+xml,%3Csvg viewBox='0 0 200 200' xmlns='http://www.w3.org/2000/svg'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='4' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E\")",
        }}
      />
      <code className="relative whitespace-nowrap font-mono text-xs leading-relaxed text-primary">
        {INSTALL_COMMAND}
      </code>
      <span className="relative shrink-0 text-primary">
        {copied ? (
          <Check className="size-3.5" aria-hidden />
        ) : (
          <Copy className="size-3.5" aria-hidden />
        )}
      </span>
    </button>
  );
}
