# Petri Logview Web Application

A React-based web application for viewing and analyzing Petri logs, built with TypeScript, Vite, and modern React ecosystem tools.

## Prerequisites

- **Node.js** (version 18 or higher)
- **npm** (comes with Node.js)

## Initial Setup

Follow these steps to set up the project from scratch:

### 1. Install Dependencies

Navigate to the project directory and install all required packages:

```powershell
# Navigate to the logview_new directory
cd .\petri\logview_new

# Install all dependencies
npm install
```

### 2. Verify Installation

After installation, you should have the following key dependencies:

**Runtime Dependencies:**

- `react` & `react-dom` - React framework
- `react-router-dom` - Client-side routing
- `@tanstack/react-query` - Data fetching and caching
- `@tanstack/react-table` - Table component library
- `@tanstack/react-virtual` - Virtual scrolling

**Development Dependencies:**

- `typescript` - TypeScript compiler
- `vite` - Build tool and dev server
- `@vitejs/plugin-react` - Vite React plugin
- `eslint` - Code linting
- `@types/react` & `@types/react-dom` - TypeScript definitions

### 3. Development Commands

```powershell
# Start development server (runs on http://localhost:3000)
npm run dev

# Build for production
npm run build

# Preview production build
npm run preview

# Run linting
npx eslint .
```

## Project Structure

```text
logview_new/
├── src/
│   ├── main.tsx          # Application entry point
│   ├── style/            # All CSS and global styles (CSS, PostCSS, or other style assets)
│   │   └── ...           # e.g. globals.css, themes, component styles
│   └── table_defs/       # Column definitions for tables used across the app
│       └── ...           # e.g. logsColumns.ts, eventsColumns.ts, devicesColumns.ts
├── index.html            # HTML template
├── package.json          # Dependencies and scripts
├── tsconfig.json         # TypeScript configuration
├── tsconfig.node.json    # TypeScript config for build tools
├── vite.config.ts        # Vite configuration
├── eslint.config.ts      # ESLint configuration
└── README.md             # This file
```

## Troubleshooting

### Common Issues

1. **Module not found errors**: Ensure all dependencies are installed with `npm install`
2. **TypeScript errors**: Make sure both `tsconfig.json` and `tsconfig.node.json` are present
3. **Port already in use**: The dev server uses port 3000 by default. You can change this in `vite.config.ts`

## Important: Build Before Pushing / Merging (Temporary Feature)

Please run a production build before pushing or opening a merge request to
ensure the front-end assets are up to date.

```powershell
npm run build
```

- Where the output goes:
  - The build artifacts will be written to the `assets/` folder at the project
    root (e.g. `logview_new/assets/`).

  - You can run `npm run preview` to verify the production build locally.

Note: This is a temporary requirement and will be automated in the future.

## Recommended: Prettier Setup for Consistent Formatting

To ensure consistent code formatting, it is recommended to install the
[Prettier](https://prettier.io/) extension in your code editor (such as VS
Code).

For VS Code users, add the following to your settings to automatically format files on save:

```json
"editor.defaultFormatter": "esbenp.prettier-vscode"
```

## Housekeeping Stuff

### 1. Updating branch quick-filters

The branches we want to track test and runs for are bound to frequently change
as new releases come through. For convenience these filters are defined as a
list in the `branch_quick_filters.tsx` file. `run_filters` will update the
filters that show up on the Runs page. `test_filters` will update the filters
that show up for the Test and TestDetails pages. `all` is a reserved branch
filter name and will display all the branches.
