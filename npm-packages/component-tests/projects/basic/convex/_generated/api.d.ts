/* eslint-disable */
/**
 * Generated `api` utility.
 *
 * THIS CODE IS AUTOMATICALLY GENERATED.
 *
 * To regenerate, run `npx convex dev`.
 * @module
 */

import type * as componentEntry from "../componentEntry.js";
import type * as errors from "../errors.js";

import type {
  ApiFromModules,
  FilterApi,
  FunctionReference,
} from "convex/server";

declare const fullApi: ApiFromModules<{
  componentEntry: typeof componentEntry;
  errors: typeof errors;
}>;

/**
 * A utility for referencing Convex functions in your app's public API.
 *
 * Usage:
 * ```js
 * const myFunctionReference = api.myModule.myFunction;
 * ```
 */
export declare const api: FilterApi<
  typeof fullApi,
  FunctionReference<any, "public">
>;

/**
 * A utility for referencing Convex functions in your app's internal API.
 *
 * Usage:
 * ```js
 * const myFunctionReference = internal.myModule.myFunction;
 * ```
 */
export declare const internal: FilterApi<
  typeof fullApi,
  FunctionReference<any, "internal">
>;

export declare const components: {
  errors: {
    throwSystemError: {
      fromAction: FunctionReference<"action", "internal", any, any>;
      fromQuery: FunctionReference<"query", "internal", any, any>;
    };
  };
  envVars: {
    messages: {
      envVarAction: FunctionReference<"action", "internal", any, any>;
      envVarQuery: FunctionReference<"query", "internal", any, any>;
      hello: FunctionReference<"action", "internal", any, any>;
      systemEnvVarAction: FunctionReference<"action", "internal", any, any>;
      systemEnvVarQuery: FunctionReference<"query", "internal", any, any>;
    };
  };
  component: {
    messages: {
      dateNow: FunctionReference<"query", "internal", {}, any>;
      hello: FunctionReference<"action", "internal", {}, any>;
      insertMessage: FunctionReference<
        "mutation",
        "internal",
        { channel: string; text: string },
        any
      >;
      listMessages: FunctionReference<"query", "internal", {}, any>;
      mathRandom: FunctionReference<"query", "internal", {}, any>;
      tryToPaginate: FunctionReference<"query", "internal", {}, any>;
    };
  };
};
