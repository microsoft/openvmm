# Pull Request Statistics Integration

This document describes the pull request statistics integration added to the OpenVMM repository to provide insights into code review activity and development velocity.

## Overview

A new workflow has been added to automatically collect and analyze pull request statistics using GitHub's official `issue-metrics` action. This provides valuable insights into the development process and team collaboration patterns.

## Workflow: PR Statistics (`pr-statistics.yml`)

### Schedule and Triggers
- **Automatic Schedule:** Weekly on Mondays at midnight UTC
- **Manual Trigger:** Available via GitHub Actions UI with configurable time period
- **Default Analysis Period:** 30 days (customizable via manual trigger)

### Features
- Uses GitHub's official `github/issue-metrics` action for reliable data collection
- Generates comprehensive markdown reports with team collaboration insights
- Automatically saves historical reports in `.github/reports/` directory
- Uploads artifacts with 90-day retention for long-term trend analysis
- Provides workflow summaries in GitHub Actions for quick overview

### Generated Reports Include
- **Pull Request Creation Rate:** Frequency and patterns of new PRs
- **Review Turnaround Time:** Average time from PR creation to first review
- **Merge Time Analysis:** Time from PR creation to successful merge
- **Reviewer Participation:** Distribution of review activity across team members
- **PR Size Analysis:** Trends in pull request size and complexity

## Key Metrics and Insights

The workflow provides actionable insights into:

### Review Velocity
- Time from PR creation to first review
- Time from first review to approval
- Overall time from creation to merge
- Patterns in review response times

### Team Collaboration
- Number of reviewers per pull request
- Review participation across team members
- Cross-team collaboration patterns
- Knowledge sharing through code reviews

### Development Efficiency
- Pull request creation frequency trends
- Average PR size and complexity metrics
- Merge success rates and patterns
- Bottlenecks in the development workflow

### Process Optimization
- Identification of review bottlenecks
- Optimal PR size recommendations
- Workload distribution insights
- Process improvement opportunities

## Accessing Reports

### Automated Reports
- **Schedule:** Generated every Monday automatically
- **Location:** Artifacts section of GitHub Actions workflow runs
- **Retention:** 90 days for historical trend analysis
- **Format:** Markdown reports with structured data

### Manual Reports
- **Trigger:** Available in GitHub Actions → PR Statistics → Run workflow
- **Customization:** Configurable analysis period (default 30 days)
- **Use Cases:** Immediate analysis, custom time periods, special reviews

### Report Structure
- **Summary Overview:** Key statistics and trends
- **Detailed Metrics:** Comprehensive analysis of PR activity
- **Historical Context:** Comparison with previous periods when available
- **Actionable Insights:** Recommendations for process improvements

## Benefits for Development Teams

### For Developers
- **Understand review patterns** and optimize PR timing
- **Track personal metrics** and contribution patterns
- **Identify optimal PR size** for faster review cycles
- **Learn from team collaboration** patterns

### for Team Leads
- **Monitor team velocity** and collaboration health
- **Balance review workload** across team members
- **Identify process bottlenecks** and improvement opportunities
- **Track progress** on development workflow optimization

### For Project Managers
- **Measure development velocity** with concrete metrics
- **Plan capacity** based on historical review patterns
- **Report on team productivity** with data-driven insights
- **Make informed decisions** about process changes

## Configuration and Customization

### Modifying the Schedule
To change the automatic report generation frequency:
1. Edit the `cron` expression in `.github/workflows/pr-statistics.yml`
2. Current schedule: `'0 0 * * 1'` (weekly on Mondays)
3. Examples:
   - Daily: `'0 0 * * *'`
   - Bi-weekly: `'0 0 */14 * *'`
   - Monthly: `'0 0 1 * *'`

### Customizing Analysis Period
Default analysis covers the last 30 days, but this can be adjusted:
- **Manual runs:** Use the "days" input when triggering manually
- **Permanent change:** Modify the `default` value in the workflow file
- **Seasonal analysis:** Use longer periods (60-90 days) for quarterly reviews

### Adding Custom Metrics
The workflow can be extended to include additional analysis:
- **GitHub CLI queries:** Add custom data collection steps
- **External integrations:** Connect with project management tools
- **Custom reporting:** Generate specialized reports for specific needs
- **Notification systems:** Add Slack/Teams notifications for significant changes

## Security and Permissions

The workflow uses minimal required permissions:
- **`contents: read`** - Access repository contents
- **`pull-requests: read`** - Access PR data for analysis
- **`issues: read`** - Comprehensive repository activity analysis

All data analyzed is already available to repository contributors, and no sensitive information is exposed or stored.

## Troubleshooting

### Common Issues

#### Missing or Incomplete Reports
- **Cause:** Low PR activity during analysis period
- **Solution:** Extend analysis period or check repository activity

#### Workflow Not Running
- **Cause:** Repository inactivity (GitHub requirement for scheduled workflows)
- **Solution:** Trigger manually or ensure regular repository activity

#### Permission Errors
- **Cause:** Insufficient token permissions
- **Solution:** Verify `GITHUB_TOKEN` has required permissions (usually automatic)

### Debugging Steps
1. **Check Workflow Logs:** Review detailed logs in GitHub Actions
2. **Verify Triggers:** Ensure scheduled or manual triggers are working
3. **Review Artifacts:** Check if artifacts are being generated correctly
4. **Test Manual Run:** Try manual trigger with different parameters

## Future Enhancements

Potential improvements to consider:
- **Dashboard Integration:** Connect with external analytics platforms
- **Trend Visualization:** Add charts and graphs for better visual insights  
- **Automated Notifications:** Alert team leads about significant changes
- **Integration with Tools:** Connect with Jira, Azure DevOps, or other PM tools
- **ML-Based Insights:** Predictive analytics for development patterns
- **Custom Metrics:** Team-specific KPIs and success measures

## Getting Started

1. **Workflow is Active:** The PR statistics workflow is already configured and will run automatically
2. **Manual Trigger:** Go to Actions → PR Statistics → Run workflow for immediate analysis
3. **Review Reports:** Check workflow run artifacts for generated reports
4. **Monitor Trends:** Regular reports will help establish baseline metrics and track improvements

---

*For questions about PR statistics or issues with the workflow, check the GitHub Actions runs or create an issue in the repository.*