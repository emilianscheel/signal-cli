import Image from "next/image";
import Link from "next/link";

import { InstallCommand } from "@/components/install-command";
import { Button } from "@/components/ui/button";

export default function Home() {
  return (
    <div className="relative flex min-h-dvh items-center justify-center overflow-hidden">
      <Image
        src="/background.png"
        alt=""
        fill
        priority
        className="object-cover object-center"
        sizes="100vw"
      />
      <div aria-hidden className="absolute inset-0 bg-[#fdf5e6]/45" />

      <main className="relative z-10 flex w-full max-w-lg flex-col items-center gap-4 px-6 py-16 text-center">
        <h1 className="text-xl tracking-tight text-primary sm:text-2xl">
          signal-cli
        </h1>

        <InstallCommand />

        <Button
          variant="secondary"
          size="sm"
          nativeButton={false}
          render={
            <Link
              href="https://github.com/emilianscheel/signal-cli"
              target="_blank"
              rel="noopener noreferrer"
            />
          }
          className="rounded-full border border-primary bg-secondary px-4 text-xs text-secondary-foreground hover:bg-primary hover:text-primary-foreground"
        >
          GitHub
        </Button>
      </main>
    </div>
  );
}
