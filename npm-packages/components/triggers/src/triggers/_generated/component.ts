/* eslint-disable */
/**
 * Generated `ComponentApi` utility.
 *
 * THIS CODE IS AUTOMATICALLY GENERATED.
 *
 * To regenerate, run `npx convex dev`.
 * @module
 */

import type { FunctionReference } from "convex/server";

/**
 * A utility for referencing a Convex component's exposed API.
 *
 * Useful when expecting a parameter like `components.myComponent`.
 * Usage:
 * ```ts
 * async function myFunction(ctx: QueryCtx, component: ComponentApi) {
 *   return ctx.runQuery(component.someFile.someQuery, { ...args });
 * }
 * ```
 */
export type ComponentApi<Name extends string | undefined = string | undefined> =
  {
    documents: {
      deleteDoc: FunctionReference<
        "mutation",
        "internal",
        { atomicDelete: string; id: string; triggers: Array<string> },
        null,
        Name
      >;
      insert: FunctionReference<
        "mutation",
        "internal",
        { atomicInsert: string; triggers: Array<string>; value: any },
        string,
        Name
      >;
      patch: FunctionReference<
        "mutation",
        "internal",
        {
          atomicPatch: string;
          id: string;
          triggers: Array<string>;
          value: any;
        },
        null,
        Name
      >;
      replace: FunctionReference<
        "mutation",
        "internal",
        {
          atomicReplace: string;
          id: string;
          triggers: Array<string>;
          value: any;
        },
        null,
        Name
      >;
    };
  };
