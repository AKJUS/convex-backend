import { Meta, StoryObj } from "@storybook/react";
import {
  AuditLogAction,
  AuditLogEventResponse,
  MemberResponse,
  Team,
} from "generatedApi";
import { AuditLogItem } from "./AuditLogItem";

const team: Team = {
  id: 1,
  slug: "team-slug",
  name: "Team Name",
  creator: 1,
  suspended: false,
};

const member: MemberResponse = {
  id: 1,
  name: "John Doe",
  email: "member@convex.dev",
};

export default {
  component: AuditLogItem,
  args: {
    team,
    memberId: 1,
    members: [member],
    projects: [],
  },
} satisfies Meta<typeof AuditLogItem>;

type Story = StoryObj<typeof AuditLogItem>;

export const SpendingLimitChange: Story = {
  args: {
    entry: {
      id: 1,
      createTime: new Date().toISOString(),
      action: "setSpendingLimit" as AuditLogAction,
      actor: { member: 1 },
      metadata: {
        previous: {
          warningThresholdCents: 500_00,
          disableThresholdCents: 1000_00,
          state: "Running",
        },
        current: {
          warningThresholdCents: 5000_00,
          disableThresholdCents: 10000_00,
          state: "Running",
        },
      },
    } as unknown as AuditLogEventResponse,
  },
};

export const SpendingLimitChangeAddAndRemove: Story = {
  args: {
    entry: {
      id: 1,
      createTime: new Date().toISOString(),
      action: "setSpendingLimit" as AuditLogAction,
      actor: { member: 1 },
      metadata: {
        previous: {
          disableThresholdCents: null,
          warningThresholdCents: 0,
          state: "Running",
        },
        current: {
          disableThresholdCents: 3200_00,
          warningThresholdCents: null,
          state: "Running",
        },
      },
    } as unknown as AuditLogEventResponse,
  },
};

export const SpendingLimitChangeOnlyOneValue: Story = {
  args: {
    entry: {
      id: 1,
      createTime: new Date().toISOString(),
      action: "setSpendingLimit" as AuditLogAction,
      actor: { member: 1 },
      metadata: {
        previous: {
          disableThresholdCents: 12345_00,
          warningThresholdCents: null,
          state: "Running",
        },
        current: {
          disableThresholdCents: 54321_00,
          warningThresholdCents: null,
          state: "Running",
        },
      },
    } as unknown as AuditLogEventResponse,
  },
};
