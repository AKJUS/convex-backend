import { StoryObj } from "@storybook/nextjs";
import { Callout } from "@ui/Callout";

export default { component: Callout };

export const Error: StoryObj<typeof Callout> = {
  args: {
    variant: "error",
    children: "This is an error",
  },
};

export const Instructions: StoryObj<typeof Callout> = {
  args: {
    children: "These are instructions",
  },
};
