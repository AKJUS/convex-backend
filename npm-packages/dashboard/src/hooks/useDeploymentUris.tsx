import { useRouter } from "next/router";
import { useTeams } from "api/teams";
import { useDefaultDevDeployment, useDeployments } from "api/deployments";
import { PROVISION_PROD_PAGE_NAME } from "@common/lib/deploymentContext";

export function useDeploymentUris(
  projectId: number,
  projectSlug: string,
  teamSlug?: string,
) {
  const router = useRouter();
  const subroute =
    router.route.split("/t/[team]/[project]/[deploymentName]")[1] || "/";
  const { selectedTeamSlug } = useTeams();

  const { deployments } = useDeployments(projectId);

  const projectURI = `/t/${teamSlug || selectedTeamSlug}/${projectSlug}`;

  const prodDeployment =
    deployments &&
    deployments.find((deployment) => deployment.deploymentType === "prod");
  const prodHref = prodDeployment
    ? `${projectURI}/${prodDeployment.name}${subroute}`
    : `${projectURI}/${PROVISION_PROD_PAGE_NAME}`;
  const devDeployment = useDefaultDevDeployment(projectId);
  const devHref = devDeployment
    ? `${projectURI}/${devDeployment.name}${subroute}`
    : undefined;

  const isProdDefault = !devDeployment;

  return {
    isLoading: !deployments,
    isProdDefault,
    prodHref,
    devHref,
    defaultHref: isProdDefault ? prodHref : devHref,
    generateHref: (deploymentName: string) =>
      `${projectURI}/${deploymentName}${subroute}`,
  };
}
