import { createContext, useContext } from 'react';
import type { DemoState, Page } from './domain/model';
import type { WorkbenchAdapter } from './domain/adapter';
import type { makeFixtures } from './domain/fixtures';

export interface DraftPreset {
  amount?: string;
  payeeQuery?: string;
  payeeId?: string;
}
export interface WorkbenchContext {
  state: DemoState;
  adapter: WorkbenchAdapter;
  fixtures: ReturnType<typeof makeFixtures>;
  now: Date;
  page: Page;
  preset?: DraftPreset;
  navigate: (page: Page, preset?: DraftPreset) => void;
  notify: (message: string, error?: boolean) => void;
}
export const Context = createContext<WorkbenchContext | null>(null);
export function useWorkbench() {
  const value = useContext(Context);
  if (!value) throw new Error('Missing workbench context');
  return value;
}
