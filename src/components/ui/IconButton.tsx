import React from "react";

interface IconButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  /** Tooltip and accessible name. */
  title: string;
  /** Highlights the icon, e.g. a starred entry. */
  active?: boolean;
}

/** Small borderless button holding a single icon. */
export const IconButton: React.FC<IconButtonProps> = ({
  title,
  active = false,
  className = "",
  type = "button",
  ...props
}) => (
  <button
    type={type}
    title={title}
    aria-label={title}
    className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${
      active
        ? "text-logo-primary hover:text-logo-primary/80"
        : "text-text/50 hover:text-logo-primary"
    } ${className}`}
    {...props}
  />
);
