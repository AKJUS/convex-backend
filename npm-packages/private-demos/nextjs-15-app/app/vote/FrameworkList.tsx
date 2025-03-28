import Link from "next/link";

export function FrameworkList({ current }: { current?: string }) {
  return (
    <ul className="leading-8">
      <li>
        • <OptionLink current={current} option="convex" />
      </li>
      <li>
        • <OptionLink current={current} option="nextjs" />
      </li>
      <li>
        • <OptionLink current={current} option="react" />
      </li>
    </ul>
  );
}

function OptionLink({ current, option }: { current?: string; option: string }) {
  return (
    <Link
      href={`/vote/${option}`}
      className={current === option ? "no-underline" : ""}
    >
      {getOptionName(option)}
    </Link>
  );
}

export function getOptionName(option: string) {
  switch (option) {
    case "convex":
      return "Convex";
    case "nextjs":
      return "Next.js";
    case "react":
      return "React";
    default:
      throw new Error("Unexpected option");
  }
}
