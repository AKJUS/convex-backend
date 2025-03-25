import React, { useId, useState } from "react";
import { ReferralState, Team } from "generatedApi";
import { TextInput } from "@common/elements/TextInput";
import {
  CheckCircledIcon,
  CheckIcon,
  ClockIcon,
  CopyIcon,
} from "@radix-ui/react-icons";
import { useTeamOrbSubscription } from "api/billing";
import { Sheet } from "dashboard-common/elements/Sheet";
import { cn } from "dashboard-common/lib/cn";
import { Callout } from "dashboard-common/elements/Callout";
import { useReferralState } from "api/referrals";
import { Loading } from "dashboard-common/elements/Loading";
import { useCopy } from "dashboard-common/lib/useCopy";
import { ReferralsBenefits } from "./ReferralsBenefits";

const MAX_REFERRALS = 5;

export function Referrals({ team }: { team: Team }) {
  const { subscription } = useTeamOrbSubscription(team.id);
  const isPaidPlan =
    subscription === undefined ? undefined : subscription !== null;

  const referralState = useReferralState(team.id);

  return (
    <ReferralsInner
      isPaidPlan={isPaidPlan}
      referralCode={team.referralCode}
      referralState={referralState.data}
    />
  );
}

export function ReferralsInner({
  isPaidPlan,
  referralCode,
  referralState,
}: {
  isPaidPlan: boolean | undefined;
  referralCode: string;
  referralState: ReferralState | undefined;
}) {
  const sourceReferralTeamName = referralState?.referredBy;
  const verifiedReferralsCount = referralState?.verifiedReferrals.length;

  return (
    <div className="flex grow flex-col gap-6 overflow-hidden">
      <div className="flex items-center gap-2">
        <h2>Referrals</h2>
      </div>

      {isPaidPlan && (
        <Callout>
          <div className="flex flex-col gap-1">
            <p>Thank you for subscribing to Convex!</p>
            <p>
              As a paid plan subscriber, you won’t get additional resources for
              referring friends or being referred by someone, but your friends
              can still get free Convex resources by using your referral link.
            </p>
          </div>
        </Callout>
      )}

      <Sheet>
        <h3>Refer friends and earn free Convex resources</h3>
        <p className="mt-1 max-w-lg text-content-secondary">
          Each time someone you referred creates their first project on Convex,
          both of your teams get the following benefits on top of your free plan
          limits.
        </p>

        <div className="my-4 flex items-center gap-4">
          <hr className="grow" />
          <p className="text-content-secondary">
            For each team that you refer (up to {MAX_REFERRALS} teams):
          </p>
          <hr className="grow" />
        </div>

        <ul className="mb-3 mt-4 grid gap-x-2 gap-y-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-[repeat(6,auto)]">
          <ReferralsBenefits />
        </ul>

        <hr className="my-4" />

        <ReferralLink referralCode={referralCode} />

        {sourceReferralTeamName ===
        undefined ? null : sourceReferralTeamName === null ? (
          <p className="text-content-secondary">
            You have not been referred.{" "}
            {isPaidPlan === false &&
              "Use a referral link to get free Convex resources!"}
          </p>
        ) : (
          <p className="text-content-secondary">
            You have been referred by{" "}
            <strong className="font-semibold">{sourceReferralTeamName}</strong>.
          </p>
        )}
      </Sheet>

      <Sheet>
        <div className="flex flex-col gap-4 text-sm">
          <h3>
            Your referrals
            {referralState !== undefined && (
              <>
                {" "}
                ({verifiedReferralsCount}/{MAX_REFERRALS})
              </>
            )}
          </h3>
          {referralState === undefined ? (
            <Loading fullHeight={false} className="h-48" />
          ) : (
            <div className="flex flex-col">
              {referralState.verifiedReferrals.length === 0 &&
              referralState.pendingReferrals.length === 0 ? (
                <p className="text-content-secondary">
                  No referrals yet. Share your referral link to get started!
                </p>
              ) : (
                <>
                  {referralState.verifiedReferrals.map((teamName, index) => (
                    <ReferralListItem
                      key={index}
                      teamName={teamName}
                      status="verified"
                    />
                  ))}
                  {referralState.pendingReferrals.map((teamName, index) => (
                    <ReferralListItem
                      key={index}
                      teamName={teamName}
                      status="pending"
                    />
                  ))}
                </>
              )}
            </div>
          )}
        </div>
      </Sheet>
    </div>
  );
}

function ReferralLink({ referralCode }: { referralCode: string }) {
  const id = useId();
  const [copied, setCopied] = useState(false);

  const copy = useCopy("Referral link");

  return (
    <div className="my-6 max-w-72">
      <TextInput
        id={id}
        value={`convex.dev/referral/${referralCode}`}
        readOnly
        label="Your referral link"
        Icon={copied ? Copied : CopyIcon}
        iconTooltip={copied ? "Copied" : "Copy to clipboard"}
        action={async () => {
          copy(`https://www.convex.dev/referral/${referralCode}`);

          setCopied(true);
          setTimeout(() => {
            setCopied(false);
          }, 3000);
        }}
      />
    </div>
  );
}

function Copied({ className }: { className?: string }) {
  return (
    <CheckIcon
      className={cn(className, "text-green-700 dark:text-green-400")}
    />
  );
}

function ReferralListItem({
  teamName,
  status,
}: {
  teamName: string;
  status: "pending" | "verified";
}) {
  return (
    <div className="flex items-center justify-between border-b py-2 last:border-b-0">
      <span className="text-sm text-content-primary">{teamName}</span>
      <span
        className={cn(
          "flex items-center gap-1 rounded-full px-2 py-1 text-sm",
          status === "verified"
            ? "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-100"
            : "bg-yellow-100 text-yellow-800 dark:bg-yellow-900 dark:text-yellow-100",
        )}
      >
        {status === "pending" ? (
          <ClockIcon className="size-4" />
        ) : (
          <CheckCircledIcon className="size-4" />
        )}

        {status === "pending" ? "Waiting for first project" : "Validated"}
      </span>
    </div>
  );
}
