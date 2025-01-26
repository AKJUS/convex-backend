import "../styles/globals.css";
import { AppProps } from "next/app";
import Head from "next/head";
import { Favicon } from "elements/Favicon";
import { ThemeConsumer } from "elements/ThemeConsumer";
import { ThemeProvider } from "next-themes";
import { useQuery } from "convex/react";
import udfs from "udfs";
import { ToastContainer } from "../elements/ToastContainer";
import {
  DeploymentApiProvider,
  DeploymentInfo,
  DeploymentInfoContext,
  WaitForDeploymentApi,
} from "../lib/deploymentContext";
import { DeploymentDashboardLayout } from "../layouts/DeploymentDashboardLayout";

export default function App({ Component, pageProps }: AppProps) {
  return (
    <>
      <Head>
        <title>Convex Dashboard</title>
        <meta name="description" content="Manage your Convex apps" />
        <Favicon />
      </Head>
      <ThemeProvider attribute="class" disableTransitionOnChange>
        <ThemeConsumer />
        <ToastContainer />
        <DeploymentInfoProvider>
          <DeploymentApiProvider deploymentOverride="local">
            <WaitForDeploymentApi>
              <div className="flex h-screen flex-col">
                <DeploymentDashboardLayout>
                  <Component {...pageProps} />
                </DeploymentDashboardLayout>
              </div>
            </WaitForDeploymentApi>
          </DeploymentApiProvider>
        </DeploymentInfoProvider>
      </ThemeProvider>
    </>
  );
}

export const deploymentInfo: DeploymentInfo = {
  ok: true,
  deploymentUrl: process.env.NEXT_PUBLIC_DEPLOYMENT_URL!,
  adminKey: process.env.NEXT_PUBLIC_ADMIN_KEY!,
  useCurrentTeam: () => ({
    id: 1,
    name: "Team",
    slug: "team",
  }),
  useTeamMembers: () => [],
  useTeamEntitlements: () => ({
    auditLogsEnabled: true,
  }),
  useCurrentUsageBanner: () => null,
  useCurrentDeployment: () => ({
    id: 1,
    name: "local",
    deploymentType: "prod",
    projectId: 1,
    kind: "local",
  }),
  useHasProjectAdminPermissions: () => true,
  useIsDeploymentPaused: () => {
    const deploymentState = useQuery(udfs.deploymentState.deploymentState);
    return deploymentState?.state === "paused";
  },
  CloudImport: ({ sourceCloudBackupId }: { sourceCloudBackupId: number }) => (
    <div>{sourceCloudBackupId}</div>
  ),
  TeamMemberLink: () => <div />,
  projectsURI: "/",
  deploymentsURI: "/",
};

function DeploymentInfoProvider({ children }: { children: React.ReactNode }) {
  if (deploymentInfo.ok && !deploymentInfo.deploymentUrl) {
    throw new Error("Missing NEXT_PUBLIC_DEPLOYMENT_URL");
  }
  if (deploymentInfo.ok && !deploymentInfo.adminKey) {
    throw new Error("Missing NEXT_PUBLIC_ADMIN_KEY");
  }
  return (
    <DeploymentInfoContext.Provider value={deploymentInfo}>
      {children}
    </DeploymentInfoContext.Provider>
  );
}
