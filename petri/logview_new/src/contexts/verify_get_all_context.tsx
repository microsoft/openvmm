import React, { createContext, useContext, useMemo, useState } from "react";

type VerifyGetAllContextValue = {
  verified: boolean;
  setVerified: (value: boolean) => void;
};

const VerifyGetAllContext = createContext<VerifyGetAllContextValue | undefined>(undefined);

export function VerifyGetAllProvider({ children }: { children: React.ReactNode }) {
  const [verified, setVerified] = useState(false);

  // useMemo prevents re-creating the object on every render (small perf win)
  const value = useMemo(
    () => ({ verified, setVerified }),
    [verified]
  );

  return (
    <VerifyGetAllContext.Provider value={value}>
      {children}
    </VerifyGetAllContext.Provider>
  );
}

export function useVerifyGetAll() {
  const ctx = useContext(VerifyGetAllContext);
  if (ctx === undefined) {
    throw new Error("useVerifyGetAll must be used within a VerifyGetAllProvider");
  }
  return ctx;
}