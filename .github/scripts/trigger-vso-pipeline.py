import click
import time
import sys
import backoff
from azure.devops.connection import Connection
from msrest.authentication import BasicAuthentication

@click.command()
@click.argument('pipeline_id', required=True)
@click.argument('token', required=True)
@click.option('--commit', default=None)
@click.option('--cancel', default=None)
@click.option('--organization', default='https://microsoft.visualstudio.com')
@click.option('--project', default='HyperVCloud')
@click.option('--debug', default=False, is_flag=True)
def main(pipeline_id: str, token: str, commit: str, cancel: str, organization: str, project: str, debug: bool):
    try:
        client = Connection(base_url=organization, creds=BasicAuthentication('', token)).clients.get_build_client()

        if cancel is not None:
            print(f'Cancelling build: {cancel}')
            build = client.update_build({'status': 'Cancelling'}, project=project, build_id=cancel)

            print(f'Final build status: {build.status}')
            return

        elif commit is None:
            raise RuntimeError('Either --commit or --cancel is required')

        build = lookup_build(client, project, pipeline_id, commit)
        if build is not None:
            print(f'Found existing build matching SHA: {commit}, reusing. URL: {organization}/{project}/_build/results?buildId={build.id}&view=results', file=sys.stderr)
        else:
            print(f'Scheduling build for SHA: {commit}', file=sys.stderr)

            build = {
                      'definition': {'id': pipeline_id},
                      'sourceBranch': 'release/lkg',
                      'templateParameters': {'OssSubmoduleCommit': commit}
                    }
            build = client.queue_build(build, project=project)
            print(f'Scheduled build: {build.id}. Url: {organization}/{project}/_build/results?buildId={build.id}&view=results', file=sys.stderr)

        with open('build-id', 'w') as fd:
            fd.write(str(build.id))# Write the build id to a file to make cancellation easier

        while build.result is None:
            time.sleep(10)

            latest = get_build_status(client, project, build.id)
            if latest.status != build.status:
                print(f'Build status changed: {build.status} -> {latest.status}', file=sys.stderr)

            build = latest

        if build.result != 'succeeded':
            print(f'Build failed. Final result: {build.result}', file=sys.stderr)
            print(f'To rerun the pipeline: Follow the above link select "Rerun failed jobs", then return to this page and select "Re-run failed jobs"', file=sys.stderr)
            sys.exit(1)

    except:
        if debug:
            import pdb
            import traceback
            traceback.print_exc()
            pdb.post_mortem()
        raise

@backoff.on_exception(backoff.expo, RuntimeError, max_time=120)
def lookup_build(client, project: str, pipeline_id: str, commit: str) -> str:
    builds = client.get_builds(project, definitions=[pipeline_id], query_order='startTimeDescending', top=1000)

    return next((e for e in builds if e.source_version == commit), None)

@backoff.on_exception(backoff.expo, RuntimeError, max_time=120)
def get_build_status(client, project: str, id: str):
    return client.get_build(project, id)

if __name__ == '__main__':
    main()
