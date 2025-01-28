import { DataView } from "dashboard-common";
import { withAuthenticatedPage } from "lib/withAuthenticatedPage";

export { getServerSideProps } from "lib/ssr";

export default withAuthenticatedPage(DataView);
