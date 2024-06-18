// Argument-validated versions of wrappers for use in system UDFs necessary
// because system UDFs are not analyzed.

import { GenericValidator, convexToJson } from "convex/values";
// This is where the alternatives are defined
import {
  // eslint-disable-next-line no-restricted-imports
  query as baseQuery,
  // eslint-disable-next-line no-restricted-imports
  mutation as baseMutation,
  // eslint-disable-next-line no-restricted-imports
  action as baseAction,
  // eslint-disable-next-line no-restricted-imports
  internalQuery as baseInternalQuery,
  // eslint-disable-next-line no-restricted-imports
  internalMutation as baseInternalMutation,
  // eslint-disable-next-line no-restricted-imports
  internalAction as baseInternalAction,
} from "../_generated/server";
import {
  // eslint-disable-next-line no-restricted-imports
  queryGeneric as baseQueryGeneric,
  // eslint-disable-next-line no-restricted-imports
  mutationGeneric as baseMutationGeneric,
  // eslint-disable-next-line no-restricted-imports
  actionGeneric as baseActionGeneric,
  // eslint-disable-next-line no-restricted-imports
  internalQueryGeneric as baseInternalQueryGeneric,
  // eslint-disable-next-line no-restricted-imports
  internalMutationGeneric as baseInternalMutationGeneric,
  // eslint-disable-next-line no-restricted-imports
  internalActionGeneric as baseInternalActionGeneric,
} from "convex/server";

import { DefaultFunctionArgs } from "convex/server";
import { performOp } from "../syscall";

type FunctionDefinition = {
  args: Record<string, GenericValidator>;
  handler: (ctx: any, args: DefaultFunctionArgs) => any;
};

type WrappedFunctionDefinition = {
  args: Record<string, GenericValidator>;
  handler: (ctx: any, args: DefaultFunctionArgs) => any;
  exportArgs(): string;
};

type Wrapper = (def: FunctionDefinition) => WrappedFunctionDefinition;

function withArgsValidated<T>(wrapper: T): T {
  return ((functionDefinition: FunctionDefinition) => {
    if (!("args" in functionDefinition)) {
      throw new Error("args validator required for system udf");
    }
    const wrap: Wrapper = wrapper as Wrapper;
    const func = wrap({
      args: functionDefinition.args,
      handler: () => {},
    });
    const argsValidatorJson = func.exportArgs();
    return wrap({
      args: functionDefinition.args,
      handler: async (ctx: any, args: any) => {
        const result = await performOp(
          "validateArgs",
          argsValidatorJson,
          convexToJson(args),
        );
        if (!result.valid) {
          throw new Error(result.message);
        }
        return functionDefinition.handler(ctx, args);
      },
    });
  }) as T;
}

export const queryGeneric = withArgsValidated(baseQueryGeneric);
export const mutationGeneric = withArgsValidated(baseMutationGeneric);
export const actionGeneric = withArgsValidated(baseActionGeneric);
export const internalQueryGeneric = withArgsValidated(baseInternalQueryGeneric);
export const internalMutationGeneric = withArgsValidated(
  baseInternalMutationGeneric,
);
export const internalActionGeneric = withArgsValidated(
  baseInternalActionGeneric,
);

// Specific to this schema.
export const query = withArgsValidated(baseQuery);
export const mutation = withArgsValidated(baseMutation);
export const action = withArgsValidated(baseAction);
export const internalQuery = withArgsValidated(baseInternalQuery);
export const internalMutation = withArgsValidated(baseInternalMutation);
export const internalAction = withArgsValidated(baseInternalAction);
