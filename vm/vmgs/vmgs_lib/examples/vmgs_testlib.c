// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Sample program to demo safely creating and manipulating vmgs files in C

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../vmgs.h"

int main()
{
    char *vmgsPath = "libtest.vmgs";
    char *testPath = "testfile_vmgs";
    enum FileId fileId = TpmNvram;
    bool useEnc = false;
    char *buf2 = NULL;
    int32_t ret = 0;
    int64_t len = 0;

    FILE *fp = fopen(testPath, "w");
    if (fp == NULL)
    {
        printf("failed to generate testfile\n");
        return -1;
    }

    char *buf1 = "abcdefghijk";
    ret = fwrite(buf1, 1, sizeof(buf1), fp);
    fclose(fp);
    if (ret != sizeof(buf1))
    {
        printf("failed to write to testfile\n");
        remove(testPath);
        return -1;
    }

    ret = create_vmgs(vmgsPath, 0, false, NULL, false);
    if (ret != 0)
    {
        printf("failed to create: %d\n", ret);
        goto err;
    }

    ret = write_vmgs(vmgsPath, testPath, fileId, NULL, false);
    if (ret != 0)
    {
        printf("failed to write: %d\n", ret);
        goto err;
    }

    ret = query_size_vmgs(vmgsPath, fileId, &len);
    if (ret != 0)
    {
        printf("unable to query size: %d\n", ret);
        goto err;
    }

    buf2 = (char *)calloc(len + 1, sizeof(char));
    if (buf2 == NULL)
    {
        ret = -1;
        printf("failed to allocate %zd bytes of memory\n", len + 1);
        goto err;
    }

    ret = read_vmgs(vmgsPath, fileId, NULL, useEnc, buf2, len);
    if (ret != 0)
    {
        printf("failed to read: %d\n", ret);
        goto err;
    }

    if (memcmp(buf1, buf2, len) != 0)
    {
        ret = -1;
        printf("comparison failed, read returned buf does not match original data\n");
        goto err;
    }

err:
    remove(testPath);
    remove(vmgsPath);
    free(buf2);
    return ret;
}
