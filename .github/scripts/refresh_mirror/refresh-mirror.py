# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import click
import time
import sys
from azure.devops.connection import Connection
from msrest.authentication import BasicAuthentication

@click.command()
@click.argument('pipeline_id', required=True)
@click.argument('token', required=True)
@click.option('--organization', default='https://microsoft.visualstudio.com')
@click.option('--project', default='HyperVCloud')
@click.option('--debug', default=False, is_flag=True)
def main(pipeline_id: str, token: str, organization: str, project: str, debug: bool):
    try:
        client = Connection(base_url=organization, creds=BasicAuthentication('', token)).clients.get_build_client()

        build = {
                    'definition': {'id': pipeline_id},
                    'templateParameters': {'branchToMirror': 'release/1.7.2511', 'branchToUpdateSubmodule': 'staging/1.7.2511', 'updateSubmodule': 'true'},
                }
        build = client.queue_build(build, project=project)
        print(f'Scheduled build: {build.id}. Url: {organization}/{project}/_build/results?buildId={build.id}&view=results', file=sys.stderr)

    except:
        if debug:
            import pdb
            import traceback
            traceback.print_exc()
            pdb.post_mortem()
        raise

if __name__ == '__main__':
    main()
