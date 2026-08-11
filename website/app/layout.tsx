import type { Metadata } from "next";
import { GeistMono } from "geist/font/mono";
import "./globals.css";

export const metadata: Metadata = {
  title: "signal-cli",
  description: "a fast, keyboard-first signal client for your terminal.",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html lang="en" className={`${GeistMono.variable} h-full antialiased`}>
      <body className="min-h-full font-mono">{children}</body>
    </html>
  );
}
